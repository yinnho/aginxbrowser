use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use deno_core::{v8, RuntimeOptions};
use crate::diting_dom::DomTree;

/// Re-exported so other crates (obscura-browser, obscura-cdp) can name the V8
/// isolate handle without taking a direct dependency on deno_core.
pub use deno_core::v8::IsolateHandle;

use crate::diting_js::module_loader::DitingModuleLoader;
use crate::diting_js::ops::{build_extension, JsState};

static SNAPSHOT: &[u8] = include_bytes!(env!("AGINXBROWSER_SNAPSHOT_PATH"));

/// CDP `Runtime.RemoteObject` shape returned by evaluate paths. Our HTTP
/// surface only reads `value`; the rest is the CDP serialization contract
/// (kept so a CDP consumer can adopt it without reshaping).
#[cfg_attr(not(test), allow(dead_code))] // tests cross-check the type metadata
#[derive(Debug, Clone)]
pub struct RemoteObjectInfo {
    pub js_type: String,
    pub subtype: Option<String>,
    pub class_name: String,
    /// CDP preview text ("Object" / "Array(3)" …). Nothing renders it yet.
    #[allow(dead_code)]
    pub description: String,
    pub object_id: Option<String>,
    pub value: Option<serde_json::Value>,
}

/// Outcome of a CDP evaluate / callFunctionOn: the remote object plus any
/// thrown (or rejected) exception. Captured separately so the CDP layer can
/// emit `Runtime.exceptionThrown` + `exceptionDetails` instead of collapsing a
/// throw into a plain `undefined`/`null` result.
#[derive(Debug, Clone)]
pub struct EvalOutcome {
    pub info: RemoteObjectInfo,
    pub exception: Option<ExceptionInfo>,
}

/// Details of a sync-thrown or awaited-rejection exception, enough to
/// synthesize CDP `Runtime.exceptionThrown` and `exceptionDetails`.
#[derive(Debug, Clone)]
pub struct ExceptionInfo {
    /// "Uncaught" for a sync throw, "Uncaught (in promise)" for a rejection.
    pub text: String,
    /// Human-readable message, e.g. "Error: boom".
    pub description: String,
    /// Error constructor name, e.g. "Error" / "TypeError".
    pub class_name: String,
    /// Object-store id of the error object, when one was allocated.
    pub object_id: Option<String>,
}

static ISOLATE_CONSTRUCT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How much the near-heap-limit callback raises the limit so V8 can unwind
/// the terminated script instead of aborting the process.
const HEAP_LIMIT_RECOVERY_HEADROOM_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct HeapLimitState {
    tripped: std::sync::atomic::AtomicBool,
    restore_limit: std::sync::atomic::AtomicUsize,
}

/// V8's default response to hitting the heap limit is to abort the whole
/// process — with many sessions in one server, one page's allocation loop
/// would kill every session. The callback terminates the current script
/// instead and lends the isolate just enough headroom to unwind.
fn install_heap_limit_guard(
    runtime: &mut deno_core::JsRuntime,
    isolate_handle: IsolateHandle,
    state: std::sync::Arc<HeapLimitState>,
) {
    runtime.add_near_heap_limit_callback(move |current_limit, _initial_limit| {
        let _ = state.restore_limit.compare_exchange(
            0,
            current_limit,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        state.tripped.store(true, std::sync::atomic::Ordering::SeqCst);
        isolate_handle.terminate_execution();
        current_limit.saturating_add(HEAP_LIMIT_RECOVERY_HEADROOM_BYTES)
    });
}

pub struct JsRuntime {
    runtime: deno_core::JsRuntime,    state: Rc<RefCell<JsState>>,
    object_store: HashMap<String, String>,
    object_counter: u64,
    /// Thread-safe handle to this runtime's V8 isolate, captured at
    /// construction. Lets a watchdog be armed from `&self` (the CDP dispatcher
    /// only holds `&Page` on the hot path) and is stable for the isolate's life.
    isolate_handle: IsolateHandle,
    /// Set by the near-heap-limit callback when it had to terminate a script.
    /// The next V8 entry point recovers the isolate (cancel termination,
    /// restore the real limit) before running more JS.
    heap_limit_state: std::sync::Arc<HeapLimitState>,
    /// How many times a watchdog had to terminate the isolate. Read before/after
    /// an event-loop pump to detect that a page is storming (a terminated
    /// microtask loop re-queues itself on the next pump, so pumping it again
    /// just re-feeds the storm).
    watchdog_fired_total: std::cell::Cell<u64>,
    /// Per-module evaluation outcome cache (upstream 4f6d256): browsers
    /// evaluate a module script exactly once per document. deno_core 0.350
    /// asserts on a second mod_evaluate of the same ModuleId instead of
    /// treating it as the spec's module-map no-op, so a page loading the same
    /// module URL twice (duplicate <script type=module src>, or a root already
    /// evaluated earlier as another graph's dependency) panics without this.
    module_evaluations: HashMap<deno_core::ModuleId, Result<(), String>>,
}

/// Handle to an armed V8 execution watchdog (see [`JsRuntime::arm_watchdog`]).
/// Holds the cancel channel and the watchdog thread; pass it back to
/// `disarm_watchdog` to stop the watchdog and learn whether it fired.
pub struct WatchdogToken {
    pair: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    join: Option<std::thread::JoinHandle<()>>,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Arm a V8 termination watchdog directly from an isolate handle, with no
/// runtime borrow. The CDP dispatcher uses this to bound every command so a
/// hung page cannot hold the process-wide V8 lock forever. Pair with
/// [`WatchdogToken::stop`]; if `stop` returns true, clear the termination flag
/// via [`JsRuntime::cancel_termination`] before reusing the isolate.
pub fn spawn_watchdog(handle: IsolateHandle, budget: std::time::Duration) -> WatchdogToken {
    let pair = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pair_c = pair.clone();
    let fired_c = fired.clone();
    let join = std::thread::spawn(move || {
        let (lock, cvar) = &*pair_c;
        let mut cancelled = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + budget;
        loop {
            // Check first: stop() may have set this (and notified into the void)
            // before this thread even started, which happens constantly for fast
            // CDP commands where stop() is called right after spawn. Without this
            // top check the lost notify means we wait the full budget before
            // noticing, and stop()'s join() blocks for that whole time.
            if *cancelled {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                fired_c.store(true, std::sync::atomic::Ordering::SeqCst);
                handle.terminate_execution();
                return;
            }
            let (guard, _) = cvar.wait_timeout(cancelled, remaining).unwrap();
            cancelled = guard;
            if *cancelled {
                return;
            }
        }
    });
    WatchdogToken { pair, join: Some(join), fired }
}

impl Drop for WatchdogToken {
    /// Cancellation safety for armed watchdogs. Async callers (the session
    /// idle pump, settle futures) can be dropped between `arm_watchdog` and
    /// `disarm_watchdog` — `tokio::select!` preempts the pump the moment a
    /// command arrives. Without this the orphaned thread later fires
    /// `terminate_execution()` into whatever runs next and bricks the
    /// isolate (no `cancel_terminate_execution` ever follows; measured as
    /// every subsequent eval failing with "Uncaught Error: execution
    /// terminated"). Dropping the token cancels the thread instead: it
    /// sleeps in `wait_timeout` while holding the mutex, so once we acquire
    /// the lock the thread can only wake, see the cancel flag and exit — it
    /// cannot have terminated in between. The remaining sliver (it fired
    /// while we were blocked on the lock) is healed by the
    /// retry-on-terminated path in `evaluate`.
    fn drop(&mut self) {
        {
            let (lock, cvar) = &*self.pair;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl WatchdogToken {
    /// Stop the watchdog. Returns true if it had already fired (terminated the
    /// isolate). The caller must then clear the termination flag via
    /// [`JsRuntime::cancel_termination`] before the next eval.
    pub fn stop(mut self) -> bool {        {
            let (lock, cvar) = &*self.pair;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether this watchdog has already terminated the isolate, without
    /// stopping it. Lets a pump loop poll for termination between slices
    /// while keeping the token alive for the final `stop()`.
    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Extract a display message from a caught panic payload (`&str`, `String`,
/// or anything else reduced to "unknown panic").
fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

impl JsRuntime {
    pub fn new() -> Self {
        Self::with_base_url("about:blank")
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self::with_base_url_and_proxy(base_url, None)
    }

    /// Construct a runtime whose ES-module loader routes dynamic imports
    /// through `proxy_url` (#139). `None` is equivalent to `with_base_url`
    /// (direct connection).
    pub fn with_base_url_and_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        let state = Rc::new(RefCell::new(JsState::new()));
        let state_clone = state.clone();
        let import_map = state.borrow().import_map.clone();

        let module_loader = Rc::new(DitingModuleLoader::with_proxy_and_import_map(
            base_url,
            proxy_url,
            import_map.clone(),
        ));

        // Serialize isolate construction process-wide: V8's JSDispatchTable
        // setup is not safe to run from several threads at once, and sessions
        // plus one-shot ops each construct on their own thread concurrently
        // (upstream obscura hit this under thread-per-connection, #430).
        let mut runtime = {
            let _construct_guard = ISOLATE_CONSTRUCT_LOCK.lock().unwrap();
            deno_core::JsRuntime::new(RuntimeOptions {
                extensions: vec![build_extension()],
                module_loader: Some(module_loader),
                startup_snapshot: Some(SNAPSHOT),
                ..Default::default()
            })
        };

        runtime.op_state().borrow_mut().put(state_clone);

        runtime
            .execute_script(
                "<diting:init>",
                "globalThis.__diting_objects = {}; globalThis.__diting_oid = 0; globalThis.__diting_init();".to_string(),
            )
            .expect("init should not fail");

        let isolate_handle = runtime.v8_isolate().thread_safe_handle();
        let heap_limit_state = std::sync::Arc::new(HeapLimitState::default());
        install_heap_limit_guard(&mut runtime, isolate_handle.clone(), heap_limit_state.clone());

        JsRuntime {
            runtime,
            state,
            object_store: HashMap::new(),
            object_counter: 0,
            isolate_handle,
            heap_limit_state,
            watchdog_fired_total: std::cell::Cell::new(0),
            module_evaluations: HashMap::new(),
        }
    }

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
        self.state.borrow_mut().http_client = Some(client);
    }

    pub fn set_dom(&self, dom: DomTree) {
        self.state.borrow_mut().dom = Some(dom);
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

    pub fn set_title(&self, title: &str) {
        self.state.borrow_mut().title = title.to_string();
    }

    /// Set the source document URL exposed as `document.referrer`
    /// (navigation referrer semantics, upstream edb1785).
    pub fn set_referrer(&self, referrer: &str) {
        self.state.borrow_mut().referrer = referrer.to_string();
    }

    #[allow(dead_code)] // CDP Network.setBlockedURLs parity — no CDP client yet
    pub fn set_blocked_urls(&self, patterns: Vec<String>) {
        self.state.borrow_mut().blocked_urls = patterns;
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        self.state.borrow_mut().pending_navigation.take()
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

    /// Drain queued console calls (level, message) captured by `op_console_msg`.
    /// The CDP layer turns each into a `Runtime.consoleAPICalled` event.
    pub fn take_pending_console_calls(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.state.borrow_mut().pending_console_calls)
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

    #[cfg_attr(not(test), allow(dead_code))] // tests cover the auto-enable hang regression
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
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; /network endpoint is the pending consumer
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

    #[cfg_attr(not(test), allow(dead_code))] // frees the fetch-{N} LRU between sessions
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

    pub fn set_user_agent(&mut self, ua: &str) {
        let escaped = ua.replace('\\', "\\\\").replace('\'', "\\'");
        // After the UA lands, refresh the platform persona (GPU pool, screen,
        // dpr, hw/memory). The runtime constructor ran __diting_init before
        // any UA was known, so the persona materialized from the linux
        // default — leaving Mesa GL strings behind a macOS UA.
        let _ = self.runtime.execute_script(
            "<set-ua>",
            format!(
                "globalThis.__diting_ua = '{}'; \
                 globalThis._fpCache = null; globalThis.__diting_hw_plat = undefined; \
                 globalThis.__diting_setPersona();",
                escaped
            ),
        );
    }
    pub fn set_language(&mut self, lang: &str) {
        let escaped = lang.replace('\\', "\\\\").replace('\'', "\\'");
        let _ = self.runtime.execute_script(
            "<set-lang>",
            format!("globalThis.__diting_lang = '{}';", escaped),
        );
    }
    /// `execute_script` that self-heals from a stray V8 termination. A
    /// watchdog that fired without a paired disarm (the pre-Drop-token
    /// cancellation race; see [`WatchdogToken`]) leaves the isolate's
    /// termination flag set, after which *every* execute fails with
    /// "Uncaught Error: execution terminated" forever. Clear the flag and
    /// retry once: the caller's expression itself didn't run yet, so one
    /// clean retry fully masks the hiccup.
    fn execute_script_retry_terminated(
        &mut self,
        name: &'static str,
        source: String,
    ) -> Result<v8::Global<v8::Value>, String> {
        match self.runtime.execute_script(name, source.clone()) {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("execution terminated") {
                    return Err(format!("JS error: {}", msg));
                }
                tracing::warn!("cleared stray V8 termination flag before {}", name);
                self.runtime.v8_isolate().cancel_terminate_execution();
                self.runtime
                    .execute_script(name, source)
                    .map_err(|e| format!("JS error: {}", e))
            }
        }
    }

    /// If the heap-limit guard terminated the last script, recover the
    /// isolate before new JS runs: cancel the termination and restore the
    /// real heap limit (the callback had inflated it to let V8 unwind).
    fn recover_heap_limit(&mut self) -> bool {
        if !self
            .heap_limit_state
            .tripped
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.runtime.v8_isolate().cancel_terminate_execution();
        let restore_limit = self
            .heap_limit_state
            .restore_limit
            .swap(0, std::sync::atomic::Ordering::SeqCst);
        self.runtime.remove_near_heap_limit_callback(restore_limit);
        install_heap_limit_guard(
            &mut self.runtime,
            self.isolate_handle.clone(),
            self.heap_limit_state.clone(),
        );
        tracing::warn!("V8 heap limit reached: terminated the current JavaScript task");
        true
    }

    pub fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, String> {
        self.recover_heap_limit();
        let wrapped = Self::wrap_expression(expression);
        let result = self.execute_script_retry_terminated("<eval>", wrapped)?;
        self.v8_to_json(result)
    }

    pub async fn evaluate_for_cdp(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<RemoteObjectInfo, String> {
        match self
            .evaluate_for_cdp_outcome(expression, return_by_value, await_promise)
            .await?
        {
            EvalOutcome {
                info,
                exception: None,
            } => Ok(info),
            EvalOutcome {
                exception: Some(exc),
                ..
            } => {
                if await_promise {
                    Err(format!("Promise rejected: {}", exc.description))
                } else {
                    // Pre-exceptionThrown behavior: a sync throw was swallowed
                    // to `undefined` so callers couldn't distinguish it from a
                    // genuine `undefined` return.
                    Ok(RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    })
                }
            }
        }
    }

    /// Like [`evaluate_for_cdp`], but a thrown/rejected expression comes back
    /// as an `EvalOutcome` carrying the exception instead of being collapsed
    /// into `Err("Promise rejected: …")` (await) or `undefined` (sync). The
    /// CDP Runtime domain consumes this to emit `Runtime.exceptionThrown` +
    /// `exceptionDetails`.
    pub async fn evaluate_for_cdp_outcome(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<EvalOutcome, String> {
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);

        // Same trailing-semicolon trim as wrap_expression — Playwright's
        // utility-script eval ends with `})();`, and `({expr})` would
        // otherwise become `(...;)` which is a parse-time SyntaxError.
        let cleaned_expr = expression
            .trim()
            .trim_end_matches(|c: char| c == ';' || c.is_whitespace());

        // Puppeteer / Playwright bundles end with a `//# sourceURL=...`
        // line comment. If we put `{expr})` on a single line the comment
        // swallows the closing paren and our wrapper breaks. A newline
        // before the `)` terminates any trailing line comment so the
        // parens close on their own line.
        let done_counter = self.object_counter;
        let exc_meta_fn = Self::exception_meta_extract_js("e");
        // Both paths set __diting_await_meta + __diting_await_rejected so the
        // read-back after the IIFE is uniform whether the expression was
        // awaited or run synchronously.
        let meta_code = if await_promise {
            format!(
                "(async function() {{\n\
                    try {{\n\
                        var __result = await (\n{expr}\n);\n\
                        globalThis.__diting_objects['{oid}'] = __result;\n\
                        globalThis.__diting_await_meta = {meta_fn};\n\
                        globalThis.__diting_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        globalThis.__diting_objects['{oid}'] = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                    }}\n\
                    globalThis.__diting_done_{done_counter} = true;\n\
                }})()",
                expr = cleaned_expr,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
                exc_meta_fn = exc_meta_fn,
                done_counter = done_counter,
            )
        } else {
            format!(
                "(function() {{\n\
                    var __result;\n\
                    try {{\n\
                        __result = (\n{expr}\n);\n\
                        globalThis.__diting_objects['{oid}'] = __result;\n\
                        globalThis.__diting_await_meta = {meta_fn};\n\
                        globalThis.__diting_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        globalThis.__diting_objects['{oid}'] = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                    }}\n\
                }})()",
                expr = cleaned_expr,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
                exc_meta_fn = exc_meta_fn,
            )
        };

        // Watchdog-bound: a runaway expression (`while(1){}`) pins the thread
        // inside V8 where the caller's tokio timeouts cannot reach. 10s is far
        // beyond any legitimate synchronous eval; on fire, the termination is
        // cancelled (isolate reusable) and surfaces as an eval error.
        //
        // NOT execute_script_retry_terminated: that helper clears the flag and
        // retries once, which would just re-enter the spin - the watchdog
        // becomes useless. Pre-clear any stray flag from an earlier watchdog
        // instead, then run the plain one-shot execute.
        self.runtime.v8_isolate().cancel_terminate_execution();
        let eval_wd = self.arm_watchdog(std::time::Duration::from_secs(10));
        let result = self
            .runtime
            .execute_script("<eval-remote>", meta_code);
        let eval_fired = self.disarm_watchdog(eval_wd);
        if eval_fired {
            let preview: String = expression.chars().take(80).collect();
            tracing::warn!("eval terminated by watchdog (ran >10s): '{}'", preview);
            return Err("eval timed out: script ran longer than 10s".to_string());
        }
        result.map_err(|e| format!("JS error: {}", e))?;

        if await_promise {
            let __t0 = std::time::Instant::now();
            let sentinel = format!("globalThis.__diting_done_{done_counter} === true");
            self.resolve_promises_until(
                |rt| rt.runtime.execute_script("<done?>", sentinel.clone())
                    .ok()
                    .and_then(|v| rt.v8_to_json(v).ok())
                    .and_then(|j| j.as_bool())
                    .unwrap_or(false),
                5000,
            ).await;
            let __dt = __t0.elapsed();
            if __dt > std::time::Duration::from_secs(1) {
                let preview: String = expression
                    .chars()
                    .take(200)
                    .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
                    .collect();
                tracing::debug!(
                    "Runtime.evaluate awaitPromise took {}ms; expr={}",
                    __dt.as_millis(), preview,
                );
            }
        }

        let rejected = self
            .runtime
            .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        let rejected = self.v8_to_json(rejected)?.as_bool().unwrap_or(false);

        if rejected {
            return self.exception_outcome(&oid, await_promise);
        }

        let info = if return_by_value {
            let read = self
                .runtime
                .execute_script("<readResult>", format!("globalThis.__diting_objects['{}']", oid))
                .map_err(|e| format!("JS error: {}", e))?;
            let json_val = self.v8_to_json(read)?;
            Self::info_from_json(&json_val)
        } else {
            let meta = self
                .runtime
                .execute_script("<readMeta>", "globalThis.__diting_await_meta".to_string())
                .map_err(|e| format!("JS error: {}", e))?;
            let meta_str = self.v8_to_json(meta)?;
            let meta_json = if let serde_json::Value::String(s) = &meta_str {
                serde_json::from_str(s).unwrap_or(meta_str)
            } else {
                meta_str
            };
            Self::info_from_meta(&meta_json, Some(oid.clone()))
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__diting_objects['{}']", oid),
        );
        Ok(EvalOutcome {
            info,
            exception: None,
        })
    }

    #[allow(dead_code)] // CDP Runtime.callFunctionOn parity; Page-level wrapper carries the allow note
    pub async fn call_function_on_for_cdp(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<RemoteObjectInfo, String> {
        match self
            .call_function_on_for_cdp_outcome(
                function_declaration,
                object_id,
                arguments,
                return_by_value,
                await_promise,
            )
            .await?
        {
            EvalOutcome {
                info,
                exception: None,
            } => Ok(info),
            EvalOutcome {
                exception: Some(exc),
                ..
            } => {
                if await_promise {
                    Err(format!("Promise rejected: {}", exc.description))
                } else {
                    Ok(RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    })
                }
            }
        }
    }

    /// Read the exception object + meta a throwing IIFE left in the globals
    /// and turn it into an `EvalOutcome` carrying the exception.
    fn exception_outcome(&mut self, oid: &str, await_promise: bool) -> Result<EvalOutcome, String> {
        let exc_meta = self
            .runtime
            .execute_script("<readExcMeta>", "globalThis.__diting_await_meta".to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        let exc_meta = self.v8_to_json(exc_meta)?;
        let exc_meta = if let serde_json::Value::String(s) = &exc_meta {
            serde_json::from_str(s).unwrap_or(exc_meta)
        } else {
            exc_meta
        };
        let class_name = exc_meta
            .get("className")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = exc_meta
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        self.object_store.insert(
            oid.to_string(),
            format!("globalThis.__diting_objects['{}']", oid),
        );

        let text = if await_promise {
            "Uncaught (in promise)"
        } else {
            "Uncaught"
        }
        .to_string();
        let info = RemoteObjectInfo {
            js_type: "object".into(),
            subtype: Some("error".into()),
            class_name: class_name.clone(),
            description: description.clone(),
            object_id: Some(oid.to_string()),
            value: None,
        };
        Ok(EvalOutcome {
            info,
            exception: Some(ExceptionInfo {
                text,
                description,
                class_name,
                object_id: Some(oid.to_string()),
            }),
        })
    }

    /// Like [`call_function_on_for_cdp`], but a throwing/rejecting function is
    /// reported as an `EvalOutcome` exception instead of being collapsed into
    /// `Err("Promise rejected: …")` or a bare `undefined`.
    pub async fn call_function_on_for_cdp_outcome(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<EvalOutcome, String> {
        let this_expr = self.resolve_this(object_id);
        let (setup, args_list) = self.build_args(arguments);

        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let exc_meta_fn = Self::exception_meta_extract_js("e");

        if await_promise {
            let done_counter = self.object_counter;
            let code = format!(
                "(async function() {{\n\
                    {setup}\n\
                    var __fn = ({fn_decl});\n\
                    var __this = ({this_expr});\n\
                    var __result;\n\
                    try {{\n\
                        __result = await __fn.call(__this, {args});\n\
                        globalThis.__diting_objects['{oid}'] = __result;\n\
                        globalThis.__diting_await_meta = {meta_fn};\n\
                        globalThis.__diting_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        globalThis.__diting_objects['{oid}'] = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                    }} finally {{\n\
                        globalThis.__diting_done_{done_counter} = true;\n\
                    }}\n\
                }})()",
                setup = setup,
                fn_decl = function_declaration,
                this_expr = this_expr,
                args = args_list,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
                exc_meta_fn = exc_meta_fn,
                done_counter = done_counter,
            );

            self.runtime
                .execute_script("<callFnAsync>", code)
                .map_err(|e| format!("JS error: {}", e))?;

            let sentinel = format!("globalThis.__diting_done_{done_counter} === true");
            self.resolve_promises_until(
                |rt| rt.runtime.execute_script("<done?>", sentinel.clone())
                    .ok()
                    .and_then(|v| rt.v8_to_json(v).ok())
                    .and_then(|j| j.as_bool())
                    .unwrap_or(false),
                5000,
            ).await;

            let rejected = self
                .runtime
                .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
                .map_err(|e| format!("JS error: {}", e))?;
            if self.v8_to_json(rejected)?.as_bool().unwrap_or(false) {
                return self.exception_outcome(&oid, true);
            }

            let info = if return_by_value {
                let read = self
                    .runtime
                    .execute_script("<readResult>", format!("globalThis.__diting_objects['{}']", oid))
                    .map_err(|e| format!("JS error: {}", e))?;
                let json_val = self.v8_to_json(read)?;
                Self::info_from_json(&json_val)
            } else {
                let meta_result = self
                    .runtime
                    .execute_script("<readMeta>", "globalThis.__diting_await_meta".to_string())
                    .map_err(|e| format!("JS error: {}", e))?;
                let meta_str = self.v8_to_json(meta_result)?;
                let meta_json = if let serde_json::Value::String(s) = &meta_str {
                    serde_json::from_str(s).unwrap_or(meta_str.clone())
                } else {
                    meta_str
                };
                Self::info_from_meta(&meta_json, Some(oid.clone()))
            };
            self.object_store.insert(
                oid.clone(),
                format!("globalThis.__diting_objects['{}']", oid),
            );
            return Ok(EvalOutcome {
                info,
                exception: None,
            });
        }

        if return_by_value {
            let code = format!(
                "(function() {{\n\
                    {setup}\n\
                    var __fn = ({fn_decl});\n\
                    var __this = ({this_expr});\n\
                    globalThis.__diting_await_rejected = false;\n\
                    try {{\n\
                        return __fn.call(__this, {args});\n\
                    }} catch(e) {{\n\
                        globalThis.__diting_objects['{oid}'] = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                        return undefined;\n\
                    }}\n\
                }})()",
                setup = setup,
                fn_decl = function_declaration,
                this_expr = this_expr,
                args = args_list,
                oid = oid,
                exc_meta_fn = exc_meta_fn,
            );
            let result = self
                .runtime
                .execute_script("<callFnByValue>", code)
                .map_err(|e| format!("JS error: {}", e))?;
            let rejected = self
                .runtime
                .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
                .map_err(|e| format!("JS error: {}", e))?;
            if self.v8_to_json(rejected)?.as_bool().unwrap_or(false) {
                return self.exception_outcome(&oid, false);
            }
            let json_val = self.v8_to_json(result)?;
            return Ok(EvalOutcome {
                info: Self::info_from_json(&json_val),
                exception: None,
            });
        }

        let code = format!(
            "(function() {{\n\
                {setup}\n\
                var __fn = ({fn_decl});\n\
                var __this = ({this_expr});\n\
                var __result;\n\
                try {{\n\
                    __result = __fn.call(__this, {args});\n\
                    globalThis.__diting_objects['{oid}'] = __result;\n\
                    globalThis.__diting_await_meta = {meta_fn};\n\
                    globalThis.__diting_await_rejected = false;\n\
                }} catch(e) {{\n\
                    globalThis.__diting_objects['{oid}'] = e;\n\
                    globalThis.__diting_await_meta = {exc_meta_fn};\n\
                    globalThis.__diting_await_rejected = true;\n\
                }}\n\
            }})()",
            setup = setup,
            fn_decl = function_declaration,
            this_expr = this_expr,
            args = args_list,
            oid = oid,
            meta_fn = Self::meta_extract_js("__result"),
            exc_meta_fn = exc_meta_fn,
        );
        let result = self
            .runtime
            .execute_script("<callFnRemote>", code)
            .map_err(|e| format!("JS error: {}", e))?;
        let rejected = self
            .runtime
            .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        if self.v8_to_json(rejected)?.as_bool().unwrap_or(false) {
            return self.exception_outcome(&oid, false);
        }
        let meta_str = self.v8_to_json(result)?;
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str.clone())
        } else {
            meta_str
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__diting_objects['{}']", oid),
        );
        Ok(EvalOutcome {
            info: Self::info_from_meta(&meta_json, Some(oid)),
            exception: None,
        })
    }
    #[cfg_attr(not(test), allow(dead_code))] // exercised via tests; CDP consumer pending
    pub async fn call_function_on(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
    ) -> Result<RemoteObjectInfo, String> {
        self.call_function_on_for_cdp(function_declaration, object_id, arguments, return_by_value, false).await
    }
    #[allow(dead_code)] // CDP Runtime.evaluate-by-object-id half of the object store
    pub fn store_object(&mut self, js_expression: &str) -> Result<String, String> {
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let code = format!(
            "globalThis.__diting_objects['{}'] = ({});",
            oid, js_expression,
        );
        self.runtime
            .execute_script("<store>", code)
            .map_err(|e| format!("Store error: {}", e))?;
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__diting_objects['{}']", oid),
        );
        Ok(oid)
    }

    #[allow(dead_code)] // ditto — store plus RemoteObject metadata extraction
    pub fn store_object_with_meta(
        &mut self,
        js_expression: &str,
    ) -> Result<RemoteObjectInfo, String> {
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let code = format!(
            "(function() {{\n\
                var __result = (\n{expr}\n);\n\
                globalThis.__diting_objects['{oid}'] = __result;\n\
                return {meta_fn};\n\
            }})()",
            expr = js_expression,
            oid = oid,
            meta_fn = Self::meta_extract_js("__result"),
        );
        let result = self
            .runtime
            .execute_script("<store-meta>", code)
            .map_err(|e| format!("Store error: {}", e))?;
        let meta_str = self.v8_to_json(result)?;
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str.clone())
        } else {
            meta_str
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__diting_objects['{}']", oid),
        );
        Ok(Self::info_from_meta(&meta_json, Some(oid)))
    }

    #[allow(dead_code)] // CDP Runtime.releaseObject parity
    pub fn release_object(&mut self, object_id: &str) {
        if self.object_store.remove(object_id).is_some() {
            let code = format!(
                "delete globalThis.__diting_objects['{}'];",
                object_id,
            );
            let _ = self.runtime.execute_script("<release>", code);
        }
    }

    #[allow(dead_code)] // CDP Runtime.releaseObjectGroup parity
    pub fn release_object_group(&mut self) {
        let _ = self.runtime.execute_script(
            "<releaseGroup>",
            "globalThis.__diting_objects = {};".to_string(),
        );
        self.object_store.clear();
    }
    pub async fn load_module(&mut self, url: &str, budget_ms: u64) -> Result<(), String> {
        let budget = tokio::time::Duration::from_millis(budget_ms);
        let specifier = deno_core::ModuleSpecifier::parse(url)
            .map_err(|e| format!("Invalid module URL {}: {}", url, e))?;

        // Fetch the module source. The old impl registered an empty string
        // and called it loaded, so every Vite / Next module bundle "loaded"
        // in 1ms with zero code and the SPA never mounted (issue #205).
        // Failures now propagate (upstream be700f5): a 404/500 must not be
        // evaluated as an empty module and reported as loaded.
        let client = self.state.borrow().http_client.clone();
        let source_code = match client {
            Some(c) => {
                let resp = c
                    .fetch(&specifier)
                    .await
                    .map_err(|e| format!("Module fetch failed ({}): {}", url, e))?;
                if !(200..=299).contains(&resp.status) {
                    return Err(format!("Module {} returned HTTP {}", url, resp.status));
                }
                crate::diting_net::decode_non_html(&resp.body, resp.content_type())
            }
            None => {
                return Err(format!(
                    "No http_client wired to runtime; cannot fetch module {}",
                    url
                ));
            }
        };

        // Bound the graph fetch: submodules resolve recursively through the
        // module loader, so a slow import chain must not hang page load.
        let module_id = match tokio::time::timeout(
            budget,
            self.runtime.load_side_es_module_from_code(
                &specifier,
                deno_core::ModuleCodeString::from(source_code),
            ),
        )
        .await
        {
            Ok(r) => r.map_err(|e| format!("Module load error: {}", e))?,
            Err(_) => {
                return Err(format!(
                    "Module graph load timed out after {}ms: {}",
                    budget_ms, url
                ));
            }
        };

        self.drive_module_eval(module_id, budget_ms, &format!("Module {}", url))
            .await
    }

    /// Drive a just-started module evaluation to completion, or up to
    /// `budget_ms`. Returns as soon as the module finishes rather than waiting
    /// for the event loop to go idle: a page timer (Vite's HMR client installs
    /// a setInterval) keeps the loop busy forever, and waiting for idle burned
    /// the whole budget on an early module and starved the one that mounts the
    /// app, leaving #root empty (upstream #374). The outcome is cached per
    /// ModuleId: browsers evaluate a module exactly once per document, and
    /// deno_core 0.350 asserts on a repeat evaluation instead of no-op'ing —
    /// the cache (plus a contained panic check) covers both duplicate roots
    /// and roots already evaluated as another graph's dependency.
    async fn drive_module_eval(
        &mut self,
        module_id: deno_core::ModuleId,
        budget_ms: u64,
        what: &str,
    ) -> Result<(), String> {
        if let Some(outcome) = self.module_evaluations.get(&module_id) {
            return outcome.clone();
        }

        let budget = tokio::time::Duration::from_millis(budget_ms);
        // deno_core 0.350 panics ("Module already evaluated") rather than
        // treating a second evaluation as the module-map no-op browsers
        // perform; that panic is a success, not a crash.
        let evaluation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.mod_evaluate(module_id)
        }));
        let result = match evaluation {
            Ok(result) => result,
            Err(payload) => {
                let message = panic_payload_message(payload);
                let outcome = if message.contains("Module already evaluated") {
                    Ok(())
                } else {
                    Err(format!("{} evaluation panicked: {}", what, message))
                };
                self.module_evaluations.insert(module_id, outcome.clone());
                return outcome;
            }
        };
        tokio::pin!(result);

        // The event-loop arm is polled first (biased): a ready loop error
        // must surface instead of being discarded while awaiting the result.
        let outcome = tokio::time::timeout(budget, async {
            let event_loop = self
                .runtime
                .run_event_loop(deno_core::PollEventLoopOptions::default());
            tokio::pin!(event_loop);
            tokio::select! {
                biased;
                e = &mut event_loop => { e?; (&mut result).await }
                r = &mut result => r,
            }
        })
        .await;

        // An eval error or timeout is returned to the page lifecycle. The
        // caller may keep rendering, but must not report the module as loaded.
        let outcome = match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{} eval error: {}", what, e)),
            Err(_) => Err(format!(
                "{} evaluation timed out after {}ms",
                what, budget_ms
            )),
        };
        self.module_evaluations.insert(module_id, outcome.clone());
        outcome
    }

    pub async fn load_inline_module(
        &mut self,
        code: &str,
        base_url: &str,
        budget_ms: u64,
    ) -> Result<(), String> {
        let budget = tokio::time::Duration::from_millis(budget_ms);
        let specifier = deno_core::ModuleSpecifier::parse(
            &format!("{}#inline-module-{}", base_url, self.object_counter),
        )
        .unwrap_or_else(|_| deno_core::ModuleSpecifier::parse("about:blank").unwrap());

        self.object_counter += 1;

        let module_id = match tokio::time::timeout(
            budget,
            self.runtime.load_side_es_module_from_code(
                &specifier,
                deno_core::ModuleCodeString::from(code.to_string()),
            ),
        )
        .await
        {
            Ok(r) => r.map_err(|e| format!("Inline module load error: {}", e))?,
            Err(_) => {
                return Err(format!(
                    "Inline module graph load timed out after {}ms",
                    budget_ms
                ));
            }
        };

        // Same completion-not-idle semantics as load_module (upstream #374):
        // an inline preamble module that installs a timer must not burn the
        // whole budget waiting for an idle that never comes.
        self.drive_module_eval(module_id, budget_ms, "Inline module").await
    }

    pub fn execute_script(&mut self, _name: &str, source: &str) -> Result<(), String> {
        self.runtime
            .execute_script("<script>", source.to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        Ok(())
    }

    pub fn execute_script_guarded(&mut self, _name: &str, source: &str) -> Result<(), String> {
        if source.len() < 10_000 {
            self.execute_script(_name, source)
        } else {
            self.execute_script_with_timeout(source, std::time::Duration::from_secs(5))
        }
    }

    pub fn execute_script_with_timeout(
        &mut self,
        source: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        if timeout.is_zero() {
            self.runtime
                .execute_script("<script>", source.to_string())
                .map_err(|e| format!("JS error: {}", e))?;
            return Ok(());
        }

        let isolate_handle = self.runtime.v8_isolate().thread_safe_handle();

        let pair = std::sync::Arc::new((
            std::sync::Mutex::new(false),
            std::sync::Condvar::new(),
        ));
        let pair_clone = pair.clone();

        let watchdog = std::thread::spawn(move || {
            let (lock, cvar) = &*pair_clone;
            let mut cancelled = lock.lock().unwrap();
            let deadline = std::time::Instant::now() + timeout;

            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    isolate_handle.terminate_execution();
                    return;
                }

                let result = cvar.wait_timeout(cancelled, remaining).unwrap();
                cancelled = result.0;
                if *cancelled {
                    return;
                }
            }
        });

        let result = self
            .runtime
            .execute_script("<script>", source.to_string());

        {
            let (lock, cvar) = &*pair;
            let mut cancelled = lock.lock().unwrap();
            *cancelled = true;
            cvar.notify_one();
        }
        let _ = watchdog.join();

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Uncaught Error: execution terminated") {
                    tracing::warn!("Script killed after {}s timeout", timeout.as_secs());
                    self.runtime.execute_script("<reset>", "undefined".to_string()).ok();
                    Ok(())
                } else {
                    Err(format!("JS error: {}", msg))
                }
            }
        }
    }

    pub async fn run_event_loop(&mut self) -> Result<(), String> {
        self.recover_heap_limit();
        self.runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .map_err(|e| format!("Event loop error: {}", e))
    }

    /// Arm a hard wall-clock backstop on synchronous V8 work. A page stuck in a
    /// synchronous loop or a microtask storm pins the OS thread inside V8, so
    /// `tokio::time::timeout` (which can only cancel at await points) never
    /// fires. This spawns a watchdog thread that terminates the isolate once
    /// `budget` elapses, forcing V8 to throw an uncatchable error and hand
    /// control back. Always balance with [`Self::disarm_watchdog`].
    pub fn arm_watchdog(&mut self, budget: std::time::Duration) -> WatchdogToken {
        spawn_watchdog(self.runtime.v8_isolate().thread_safe_handle(), budget)
    }

    /// Stop a watchdog armed by [`Self::arm_watchdog`]. If it had already fired
    /// (terminated the isolate), clear V8's termination flag so the isolate is
    /// usable again, and return `true`.
    pub fn disarm_watchdog(&mut self, token: WatchdogToken) -> bool {
        let fired = token.stop();
        if fired {
            self.runtime.v8_isolate().cancel_terminate_execution();
            self.watchdog_fired_total
                .set(self.watchdog_fired_total.get() + 1);
            tracing::warn!("V8 watchdog fired: terminated a synchronous overrun");
        }
        fired
    }

    /// Total isolate terminations so far (see `watchdog_fired_total`).
    pub fn watchdog_fired_total(&self) -> u64 {
        self.watchdog_fired_total.get()
    }

    /// This runtime's V8 isolate handle (captured at construction, stable for
    /// the isolate's life). Lets the CDP dispatcher arm a per-command watchdog
    /// from `&self`.
    #[allow(dead_code)] // CDP per-command watchdog plumbing — the CDP server itself is not absorbed
    pub fn isolate_handle(&self) -> IsolateHandle {
        self.isolate_handle.clone()
    }

    /// Clear V8's termination flag after a watchdog armed externally (via the
    /// isolate handle) fired, so the isolate is usable for the next command.
    /// No-op when the isolate is not terminating.
    #[allow(dead_code)] // ditto — the watchdog-clearing half
    pub fn cancel_termination(&mut self) {
        self.runtime.v8_isolate().cancel_terminate_execution();
    }

    /// Drive the event loop for at most `budget_ms`, bounded against BOTH async
    /// idle (tokio timeout) and synchronous hangs (V8 watchdog). A microtask
    /// storm that pins the thread is terminated ~500ms past the budget; a
    /// well-behaved page returns as soon as the loop goes idle.
    pub async fn run_event_loop_bounded(&mut self, budget_ms: u64) -> Result<(), String> {
        if budget_ms == 0 {
            return self.run_event_loop().await;
        }
        let budget = std::time::Duration::from_millis(budget_ms);
        let token = self.arm_watchdog(budget + std::time::Duration::from_millis(500));
        let result = tokio::time::timeout(budget, self.run_event_loop()).await;
        self.disarm_watchdog(token);
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) if e.contains("execution terminated") => Ok(()),
            Ok(Err(e)) => Err(e),
            // tokio idle-timeout is the normal "settled" exit, not an error.
            Err(_) => Ok(()),
        }
    }

    /// Drive the event loop until it goes idle (no pending ops, tasks, or
    /// timers), capped at `max_ms`. Returns `true` if the loop actually went
    /// idle within the budget — `false` means work was still in flight (a
    /// long fetch, an interval timer, or a synchronous overrun terminated by
    /// the watchdog). Unlike [`Self::run_event_loop_bounded`], the caller can
    /// tell "settled" from "still busy", which is what click/transition flows
    /// need: a client-side route change is only done when the flight fetch,
    /// parse, render, and pushState have all drained.
    pub async fn run_event_loop_until_idle(&mut self, max_ms: u64) -> bool {
        let budget = std::time::Duration::from_millis(max_ms);
        let token = self.arm_watchdog(budget + std::time::Duration::from_millis(500));
        let result = tokio::time::timeout(budget, self.run_event_loop()).await;
        let fired = self.disarm_watchdog(token);
        matches!(result, Ok(Ok(()))) && !fired
    }

    /// Like [`Self::evaluate`] but bounded by a V8 watchdog, so a `--eval`
    /// expression that loops forever (or awaits a promise that never settles in
    /// synchronous form) cannot hang the process.
    pub fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        if timeout.is_zero() {
            return self.evaluate(expression);
        }
        self.recover_heap_limit();
        let wrapped = Self::wrap_expression(expression);
        let token = self.arm_watchdog(timeout);
        let result = self.runtime.execute_script("<eval>", wrapped);
        let fired = self.disarm_watchdog(token);
        match result {
            Ok(v) if !fired => self.v8_to_json(v),
            Ok(_) => Err("eval timed out".to_string()),
            Err(e) => {
                let msg = e.to_string();
                if fired || msg.contains("execution terminated") {
                    Err("eval timed out".to_string())
                } else {
                    Err(format!("JS error: {}", msg))
                }
            }
        }
    }

    #[allow(dead_code)] // generic promise settle; evaluate paths settle through their own bounded loops
    pub async fn resolve_promises(&mut self) {
        // Default settle: just pump until idle or 5s.
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            self.runtime.run_event_loop(deno_core::PollEventLoopOptions::default()),
        ).await;
    }

    /// Pump the event loop until `done_check` returns true (e.g. an IIFE
    /// has written its result sentinel), or `max_total_ms` elapses.
    ///
    /// Why this exists: `run_event_loop(default)` only returns when there is
    /// no pending work. Page JS routinely schedules long setTimeouts
    /// (IntersectionObserver re-fires at 7s, requestIdleCallback, etc.) that
    /// the caller does not care about. With the plain timeout we waited 5s
    /// even when the IIFE we cared about resolved in <1ms — the click flow
    /// added ~7s per click because Puppeteer's `isIntersectingViewport`
    /// disconnects its observer in the callback, but our scheduled
    /// re-fires keep the event loop "busy" until they all fire.
    pub async fn resolve_promises_until<F>(&mut self, mut done_check: F, max_total_ms: u64)
    where
        F: FnMut(&mut Self) -> bool,
    {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(max_total_ms);
        let mut tick_ms: u64 = 1;
        // The tokio timeout below only fires between slices; a promise callback
        // that spins synchronously (or a microtask storm) pins the thread
        // INSIDE run_event_loop where the timeout cannot reach. Bound the
        // whole wait with the V8 watchdog: on fire, exit early (disarm cancels
        // the termination so the isolate stays usable).
        let wd = self.arm_watchdog(std::time::Duration::from_millis(max_total_ms + 500));
        loop {
            if done_check(self) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // Pump for a short slice. If the loop returns idle in <tick_ms,
            // run_event_loop returns Ok and we check the predicate again.
            let _ = tokio::time::timeout(
                tokio::time::Duration::from_millis(tick_ms),
                self.runtime.run_event_loop(deno_core::PollEventLoopOptions::default()),
            ).await;
            if wd.fired() {
                break;
            }
            // Backoff so a hung promise doesn't burn CPU. Caps at 50ms;
            // worst case we miss the result by <50ms.
            if tick_ms < 50 { tick_ms = (tick_ms * 2).min(50); }
        }
        if self.disarm_watchdog(wd) {
            tracing::warn!("promise wait terminated by watchdog (sync spin in event loop)");
        }
    }
    #[cfg_attr(not(test), allow(dead_code))] // used by suspend_js/resume_js lifecycle tests
    pub fn take_dom(&self) -> Option<DomTree> {
        self.state.borrow_mut().dom.take()
    }

    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        let state = self.state.borrow();
        state.dom.as_ref().map(f)
    }

    #[allow(dead_code)] // borrow-preserving DOM read; with_dom covers current callers
    pub fn dom_ref(&self) -> Option<std::cell::Ref<'_, Option<DomTree>>> {
        let r = self.state.borrow();
        if r.dom.is_some() {
            Some(std::cell::Ref::map(r, |s| &s.dom))
        } else {
            None
        }
    }
    fn make_oid(&self, counter: u64) -> String {
        format!("{{\"injectedScriptId\":1,\"id\":{}}}", counter)
    }

    fn wrap_expression(expression: &str) -> String {
        let trimmed = expression.trim();

        let is_multi_statement = trimmed.starts_with("var ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("if ")
            || trimmed.starts_with("for ")
            || trimmed.starts_with("while ")
            || trimmed.starts_with("return ");

        if is_multi_statement {
            format!(
                "(function() {{ try {{\n{}\n}} catch(e) {{ return null; }} }})()",
                expression
            )
        } else {
            // Strip trailing semicolons + whitespace before wrapping in
            // `return (...);`. Playwright's utility-script expression is
            // an IIFE that ends with `})();` — leaving the `;` in place
            // produces `return (...;);`, a SyntaxError. The script fails
            // to parse, the catch never fires (parse errors are not
            // catchable), and the function silently returns `undefined`.
            // Stripping makes the wrapped expression syntactically valid.
            //
            // The newline before the trailing `)` also terminates any
            // `//# sourceURL=...` line comment the caller may have appended
            // (Puppeteer's evaluated bundles do).
            let cleaned = trimmed.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
            format!(
                "(function() {{ try {{ return (\n{}\n); }} catch(e) {{ return null; }} }})()",
                cleaned
            )
        }
    }

    fn meta_extract_js(var_name: &str) -> String {
        format!(
            r#"(function(v) {{
                var t = typeof v;
                var st = null, cn = '', desc = '';
                if (v === null) {{ t = 'object'; st = 'null'; }}
                else if (v === undefined) {{ t = 'undefined'; }}
                else if (Array.isArray(v)) {{
                    st = 'array'; cn = 'Array';
                    desc = 'Array(' + v.length + ')';
                }}
                else if (t === 'object' && typeof v._nid === 'number') {{
                    st = 'node';
                    cn = v.constructor ? v.constructor.name : 'Node';
                    if (v.nodeType === 9) cn = 'HTMLDocument';
                    else if (v.nodeType === 1) cn = 'HTML' + (v.tagName || 'Element').charAt(0) + (v.tagName || 'Element').slice(1).toLowerCase() + 'Element';
                    desc = v.tagName ? v.tagName.toLowerCase() : (v.nodeName || 'node');
                }}
                else if (t === 'function') {{
                    cn = 'Function';
                    desc = v.name ? 'function ' + v.name + '()' : 'function()';
                }}
                else if (t === 'object') {{
                    cn = (v.constructor && v.constructor.name) || 'Object';
                    desc = cn;
                }}
                else {{ desc = String(v); }}
                return JSON.stringify({{type:t,subtype:st,className:cn,description:desc}});
            }})({var_name})"#,
            var_name = var_name,
        )
    }

    /// Extract an exception's constructor name + message as a JSON blob, the
    /// same shape `info_from_meta` reads. `meta_extract_js` stops at the
    /// constructor name ("Error") — it never pulls the message, so a thrown
    /// `new Error('boom')` would otherwise surface as description "Error"
    /// instead of "Error: boom", which is what Chrome's `exceptionDetails`
    /// reports.
    fn exception_meta_extract_js(var_name: &str) -> String {
        format!(
            r#"(function(e) {{
                var name = '', msg = '', desc = '';
                if (e !== null && e !== undefined) {{
                    if (typeof e === 'object' || typeof e === 'function') {{
                        name = e.name || (e.constructor && e.constructor.name) || '';
                        if (typeof e.message === 'string') msg = e.message;
                    }} else {{
                        try {{ msg = String(e); }} catch (_) {{}}
                    }}
                }}
                if (msg) {{ desc = name ? (name + ': ' + msg) : msg; }}
                else {{ desc = name || msg || 'Uncaught exception'; }}
                return JSON.stringify({{className:name, description:desc}});
            }})({var_name})"#,
            var_name = var_name,
        )
    }

    #[cfg_attr(not(test), allow(dead_code))] // helper of call_function_on (test-exercised)
    fn resolve_this(&self, object_id: Option<&str>) -> String {
        match object_id {
            Some(oid) => {
                if let Some(retrieval) = self.object_store.get(oid) {
                    retrieval.clone()
                } else if oid.starts_with("node-") {
                    let nid = oid.strip_prefix("node-").unwrap_or("0");
                    format!(
                        "(function() {{ \
                            var nid = {}; \
                            var cache = globalThis._cache || new Map(); \
                            if (cache.has(nid)) return cache.get(nid); \
                            return null; \
                        }})()",
                        nid
                    )
                } else {
                    "globalThis".to_string()
                }
            }
            None => "globalThis".to_string(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // helper of call_function_on (test-exercised)
    fn build_args(&self, arguments: &[serde_json::Value]) -> (String, String) {
        let mut setup_lines = Vec::new();
        let mut arg_names = Vec::new();

        for (i, arg) in arguments.iter().enumerate() {
            let arg_name = format!("__arg{}", i);
            if let Some(value) = arg.get("value") {
                let json_str = serde_json::to_string(value).unwrap_or_else(|_| "undefined".to_string());
                setup_lines.push(format!("var {} = {};", arg_name, json_str));
            } else if let Some(oid) = arg.get("objectId").and_then(|v| v.as_str()) {
                if let Some(retrieval) = self.object_store.get(oid) {
                    setup_lines.push(format!("var {} = {};", arg_name, retrieval));
                } else {
                    setup_lines.push(format!("var {} = undefined;", arg_name));
                }
            } else if let Some(unser) = arg.get("unserializableValue").and_then(|v| v.as_str()) {
                setup_lines.push(format!("var {} = {};", arg_name, unser));
            } else {
                setup_lines.push(format!("var {} = undefined;", arg_name));
            }
            arg_names.push(arg_name);
        }

        (setup_lines.join("\n"), arg_names.join(", "))
    }

    fn v8_to_json(
        &mut self,
        result: deno_core::v8::Global<deno_core::v8::Value>,
    ) -> Result<serde_json::Value, String> {
        let scope = &mut self.runtime.handle_scope();
        let local = deno_core::v8::Local::new(scope, result);

        if local.is_undefined() || local.is_null() {
            return Ok(serde_json::Value::Null);
        }
        if local.is_boolean() {
            return Ok(serde_json::Value::Bool(local.boolean_value(scope)));
        }
        if local.is_number() {
            let n = local.number_value(scope).unwrap_or(0.0);
            return Ok(serde_json::json!(n));
        }
        if local.is_string() {
            let s = local.to_rust_string_lossy(scope);
            return Ok(serde_json::Value::String(s));
        }

        let global = scope.get_current_context().global(scope);
        let json_obj_str = deno_core::v8::String::new(scope, "JSON").unwrap();
        if let Some(json_obj) = global.get(scope, json_obj_str.into()) {
            if let Some(json_obj) = json_obj.to_object(scope) {
                let stringify_str = deno_core::v8::String::new(scope, "stringify").unwrap();
                if let Some(stringify_fn) = json_obj.get(scope, stringify_str.into()) {
                    if let Ok(stringify_fn) =
                        deno_core::v8::Local::<deno_core::v8::Function>::try_from(stringify_fn)
                    {
                        let args = [local];
                        if let Some(result) = stringify_fn.call(scope, json_obj.into(), &args) {
                            let json_str = result.to_rust_string_lossy(scope);
                            if let Ok(val) = serde_json::from_str(&json_str) {
                                return Ok(val);
                            }
                        }
                    }
                }
            }
        }

        let s = local.to_rust_string_lossy(scope);
        Ok(serde_json::Value::String(s))
    }

    fn info_from_json(value: &serde_json::Value) -> RemoteObjectInfo {
        match value {
            serde_json::Value::Null => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: Some("null".into()),
                class_name: String::new(),
                description: "null".into(),
                object_id: None,
                value: Some(serde_json::Value::Null),
            },
            serde_json::Value::Bool(b) => RemoteObjectInfo {
                js_type: "boolean".into(),
                subtype: None,
                class_name: String::new(),
                description: b.to_string(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Number(n) => RemoteObjectInfo {
                js_type: "number".into(),
                subtype: None,
                class_name: String::new(),
                description: n.to_string(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::String(s) => RemoteObjectInfo {
                js_type: "string".into(),
                subtype: None,
                class_name: String::new(),
                description: s.clone(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Array(arr) => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: Some("array".into()),
                class_name: "Array".into(),
                description: format!("Array({})", arr.len()),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Object(_) => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: None,
                class_name: "Object".into(),
                description: "Object".into(),
                object_id: None,
                value: Some(value.clone()),
            },
        }
    }

    fn info_from_meta(
        meta: &serde_json::Value,
        object_id: Option<String>,
    ) -> RemoteObjectInfo {
        let js_type = meta
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("undefined")
            .to_string();
        let subtype = meta
            .get("subtype")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let class_name = meta
            .get("className")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = meta
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let value = if js_type != "object" && js_type != "function" {
            meta.get("description")
                .and_then(|v| v.as_str())
                .map(|s| serde_json::Value::String(s.to_string()))
        } else {
            None
        };

        RemoteObjectInfo {
            js_type,
            subtype,
            class_name,
            description,
            object_id,
            value,
        }
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diting_dom::parse_html;

    fn setup_runtime(html: &str) -> JsRuntime {
        let dom = parse_html(html);
        let rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt
    }

    #[test]
    fn test_document_title() {
        let mut rt = setup_runtime("<html><head><title>Test</title></head><body></body></html>");
        let title = rt.evaluate("document.title").unwrap();
        assert_eq!(title, serde_json::json!("Test Page"));
    }

    #[test]
    fn test_document_url() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let url = rt.evaluate("document.URL").unwrap();
        assert_eq!(url, serde_json::json!("http://example.com/test"));
    }

    #[test]
    fn test_query_selector() {
        let mut rt = setup_runtime("<html><body><h1>Hello</h1><p>World</p></body></html>");
        let text = rt.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(text, serde_json::json!("Hello"));
    }

    #[test]
    fn test_query_selector_all() {
        let mut rt = setup_runtime("<ul><li>A</li><li>B</li><li>C</li></ul>");
        let count = rt.evaluate("document.querySelectorAll('li').length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 3);
    }

    #[test]
    fn test_get_element_by_id() {
        let mut rt = setup_runtime(r#"<div id="test">Content</div>"#);
        let tag = rt.evaluate("document.getElementById('test').tagName").unwrap();
        assert_eq!(tag, serde_json::json!("DIV"));
    }

    #[test]
    fn document_fragment_get_element_by_id_searches_descendants() {
        let mut rt = setup_runtime(r#"<div id="target">document</div>"#);
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const frag = document.createDocumentFragment();
                    const section = document.createElement('section');
                    section.innerHTML = '<div><span id="target">fragment</span></div><p id="a.b">literal</p>';
                    frag.appendChild(section);

                    const dup = document.createDocumentFragment();
                    const deepParent = document.createElement('div');
                    deepParent.innerHTML = '<span id="dup">deep</span>';
                    const shallow = document.createElement('p');
                    shallow.id = 'dup';
                    shallow.textContent = 'shallow';
                    dup.appendChild(deepParent);
                    dup.appendChild(shallow);

                    return [
                        frag.getElementById('target').textContent,
                        frag.getElementById('missing') === null,
                        frag.getElementById('a.b').textContent,
                        frag.getElementById(123) === null,
                        dup.getElementById('dup').textContent,
                    ];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["fragment", true, "literal", true, "deep"])
        );
    }

    #[test]
    fn test_inner_html() {
        let mut rt = setup_runtime(r#"<div id="x"><p>Hello</p></div>"#);
        let html = rt.evaluate("document.getElementById('x').innerHTML").unwrap();
        assert!(html.as_str().unwrap().contains("<p>"));
    }

    #[test]
    fn test_script_execution() {
        let mut rt = setup_runtime("<ul><li>A</li><li>B</li></ul>");
        rt.execute_script(
            "test",
            r#"
            globalThis.__result = [];
            document.querySelectorAll('li').forEach(function(el) {
                globalThis.__result.push(el.textContent);
            });
        "#,
        )
        .unwrap();
        let result = rt.evaluate("globalThis.__result").unwrap();
        assert_eq!(result, serde_json::json!(["A", "B"]));
    }

    /// Regression test for #147: a TypeError in one script must not poison
    /// the runtime so that subsequent scripts (or DOM queries) collapse to
    /// empty. The reporter saw `--dump text` return 1 byte after offside.js
    /// crashed; that cascade should never happen.
    #[test]
    fn script_typeerror_does_not_poison_subsequent_execution() {
        let mut rt = setup_runtime(
            "<html><body><p id=hit>BODY_TEXT</p></body></html>",
        );

        // 1. First script throws the same flavor of error offside.js produced
        //    (`Cannot read properties of undefined (reading 'classList')`).
        let err = rt
            .execute_script("buggy", "var x; x.classList.add('y');")
            .unwrap_err();
        assert!(err.contains("classList") || err.contains("undefined"),
                "expected classList/undefined error, got: {}", err);

        // 2. The runtime must still be usable: a follow-up script runs.
        rt.execute_script("ok", "globalThis.__after_error = 'still alive';")
            .unwrap();
        let result = rt.evaluate("globalThis.__after_error").unwrap();
        assert_eq!(result, serde_json::json!("still alive"));

        // 3. DOM queries still work after the script error.
        let text = rt
            .evaluate("document.querySelector('#hit').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("BODY_TEXT"));
    }

    /// Regression for #105: `element.querySelector` and `querySelectorAll`
    /// must scope to the receiver's subtree, not the whole document.
    #[test]
    fn element_query_selector_is_scoped_to_subtree() {
        let mut rt = setup_runtime(
            r#"<div id="a"><span class="x">in a</span></div><div id="b"><span class="x">in b</span></div>"#,
        );
        let text = rt
            .evaluate("document.getElementById('a').querySelector('.x').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("in a"));

        let count_in_a = rt
            .evaluate("document.getElementById('a').querySelectorAll('.x').length")
            .unwrap();
        assert_eq!(count_in_a.as_f64().unwrap() as i64, 1);

        // Document-scoped query still sees both.
        let count_doc = rt.evaluate("document.querySelectorAll('.x').length").unwrap();
        assert_eq!(count_doc.as_f64().unwrap() as i64, 2);
    }

    /// Regression for #105: `document.forms` / `images` / `links` must be
    /// live, not hardcoded `[]`. jQuery 1.x's submit-event setup iterates
    /// `document.forms` and crashes when it's empty for pages that have forms.
    #[test]
    fn document_forms_images_links_are_live() {
        let mut rt = setup_runtime(
            r#"<form></form><form></form><img><a href="x">l</a><a>no-href</a>"#,
        );
        assert_eq!(rt.evaluate("document.forms.length").unwrap().as_f64().unwrap() as i64, 2);
        assert_eq!(rt.evaluate("document.images.length").unwrap().as_f64().unwrap() as i64, 1);
        assert_eq!(rt.evaluate("document.links.length").unwrap().as_f64().unwrap() as i64, 1);
    }

    /// Regression for #105: `HTMLFormElement` must expose `.elements` so
    /// frameworks that probe form field collections work.
    #[test]
    fn html_form_element_exposes_elements_collection() {
        let mut rt = setup_runtime(
            r#"<form id="f"><input name=a><input name=b><textarea></textarea></form>"#,
        );
        let n = rt
            .evaluate("document.getElementById('f').elements.length")
            .unwrap();
        assert_eq!(n.as_f64().unwrap() as i64, 3);
        let is_form = rt
            .evaluate("document.getElementById('f') instanceof HTMLFormElement")
            .unwrap();
        assert_eq!(is_form, serde_json::json!(true));
    }

    /// Regression for #105: `Element.prepend` must actually insert at the
    /// start, not silently no-op.
    #[test]
    fn element_prepend_inserts_at_start() {
        let mut rt = setup_runtime(r#"<div id="c"><span>existing</span></div>"#);
        rt.evaluate(
            r#"
            const c = document.getElementById('c');
            const n = document.createElement('span');
            n.id = 'first';
            c.prepend(n);
            "#,
        )
        .unwrap();
        let first_id = rt.evaluate("document.getElementById('c').firstChild.id").unwrap();
        assert_eq!(first_id, serde_json::json!("first"));
        let count = rt.evaluate("document.getElementById('c').childNodes.length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 2);
    }

    /// Regression for #105: `isEqualNode` compares structure, not identity.
    /// Framework diff algorithms rely on this.
    #[test]
    fn is_equal_node_does_structural_compare() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                const a = document.createElement('div'); a.setAttribute('class', 'x'); a.innerHTML = '<span>hi</span>';
                const b = document.createElement('div'); b.setAttribute('class', 'x'); b.innerHTML = '<span>hi</span>';
                const c = document.createElement('div'); c.innerHTML = '<span>bye</span>';
                return [a.isEqualNode(b), a.isEqualNode(c), a.isSameNode(b)];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, false, false]));
    }

    /// Regression for the long-standing insert_before arg-order bug noted
    /// in CLAUDE.md: bootstrap.js was passing (parent, new, ref) but `_dom`
    /// forwards only two args, silently dropping `ref`. With the fix,
    /// `insertBefore` actually inserts.
    #[test]
    fn insert_before_inserts_node_at_correct_position() {
        let mut rt = setup_runtime(r#"<div id="p"><span id="b">b</span><span id="c">c</span></div>"#);
        let order = rt
            .evaluate(
                r#"
                const p = document.getElementById('p');
                const a = document.createElement('span');
                a.id = 'a';
                p.insertBefore(a, document.getElementById('b'));
                return Array.from(p.children).map(e => e.id).join(',');
                "#,
            )
            .unwrap();
        assert_eq!(order, serde_json::json!("a,b,c"));
    }

    #[test]
    fn test_console_log() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script("test", "console.log('Hello from V8!')").unwrap();
    }

    #[test]
    fn test_console_calls_are_queued_for_cdp() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "test",
            "console.log('hello'); console.warn('careful'); console.error('boom')",
        )
        .unwrap();
        assert_eq!(
            rt.take_pending_console_calls(),
            vec![
                ("log".to_string(), "hello".to_string()),
                ("warn".to_string(), "careful".to_string()),
                ("error".to_string(), "boom".to_string()),
            ]
        );
        // take() drains: a second take sees nothing, so the CDP layer can't
        // re-emit the same console line on the next dispatch.
        assert!(rt.take_pending_console_calls().is_empty());
    }

    #[test]
    fn test_location() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let href = rt.evaluate("location.href").unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/test"));
    }

    #[test]
    fn test_button_click_dispatches_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt.evaluate(r#"
            const button = document.getElementById('go');
            button.addEventListener('click', () => { button.dataset.clicked = 'yes'; });
            button.click();
            return button.dataset.clicked;
        "#).unwrap();
        assert_eq!(result, serde_json::json!("yes"));
    }

    #[test]
    fn test_dispatch_mouse_event_runs_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt.evaluate(r#"
            const button = document.getElementById('go');
            let count = 0;
            button.addEventListener('click', () => { count += 1; });
            button.dispatchEvent(new MouseEvent('click', { bubbles: true }));
            return count;
        "#).unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 1);
    }

    #[test]
    fn test_location_href_assignment_updates_navigation_state() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let href = rt.evaluate("const next = '/next'; location.href = next; return location.href;").unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/next"));
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/next".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_location_reload_triggers_navigation() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // reload() used to be a no-op, so a challenge that reloaded after
        // setting a token cookie never re-fetched. It now navigates to the
        // current href like assign/replace.
        rt.evaluate("location.reload();").unwrap();
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/test".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_structured_clone_preserves_buffers_and_collections() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // The old JSON.parse(JSON.stringify) fallback dropped ArrayBuffer and
        // TypedArray to {}. Real structuredClone keeps them intact.
        let result = rt.evaluate(r#"
            const ab = new ArrayBuffer(4);
            new Uint8Array(ab).set([1, 2, 3, 4]);
            const c = structuredClone({
                buf: ab,
                view: new Uint16Array([5, 6]),
                map: new Map([["k", new Uint8Array([7])]]),
                set: new Set([8]),
                date: new Date(0),
                re: /ab+c/gi,
            });
            return [
                c.buf instanceof ArrayBuffer,
                Array.from(new Uint8Array(c.buf)),
                c.view instanceof Uint16Array,
                Array.from(c.view),
                Array.from(c.map.get("k")),
                c.set.has(8),
                c.date.getTime(),
                c.re.source,
                c.re.flags,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, [1,2,3,4], true, [5,6], [7], true, 0, "ab+c", "gi"])
        );
    }

    #[test]
    fn test_structured_clone_handles_cycles_and_error_cause() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Cycles preserve identity; an Error whose `cause` points back at
        // itself must not recurse until stack overflow.
        let result = rt.evaluate(r#"
            const obj = { name: 'a' };
            obj.self = obj;
            const c = structuredClone(obj);
            const cycleOk = c.self === c && c.name === 'a' && c !== obj;

            const err = new Error('boom');
            err.cause = err;
            const ec = structuredClone(err);
            const causeOk = ec.cause === ec && ec.message === 'boom';
            return [cycleOk, causeOk];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    #[test]
    fn test_structured_clone_own_proto_and_function_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // An own enumerable `__proto__` data property (what JSON.parse yields)
        // must clone as an own property, not reparent the clone. Functions are
        // not structured-cloneable and must throw DataCloneError.
        let result = rt.evaluate(r#"
            const obj = JSON.parse('{"__proto__": {"x": 1}, "y": 2}');
            const c = structuredClone(obj);
            const protoOk = Object.getPrototypeOf(c) === Object.prototype
                && c.y === 2
                && c.__proto__.x === 1;

            let threw = false;
            try { structuredClone({ f: function() {} }); } catch (e) {
                threw = e instanceof DOMException && e.name === "DataCloneError";
            }
            return [protoOk, threw];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_subtle_digest_variants_and_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // SHA-512/224 and SHA-512/256 were silently falling through to SHA-256,
        // and unknown names (MD5) returned a SHA-256 hash with no error. Verify
        // the FIPS 180-4 test vectors and the NotSupportedError rejection.
        let script = r#"async () => {
            const hex = (buf) => Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, '0')).join('');
            const enc = new TextEncoder();
            const sha256 = hex(await crypto.subtle.digest('SHA-256', enc.encode('abc')));
            const sha512_224 = hex(await crypto.subtle.digest('SHA-512/224', enc.encode('abc')));
            const sha512_256 = hex(await crypto.subtle.digest('SHA-512/256', enc.encode('abc')));
            let threw = false;
            try { await crypto.subtle.digest('MD5', enc.encode('abc')); } catch (e) {
                threw = e.name === 'NotSupportedError';
            }
            return [sha256, sha512_224, sha512_256, threw];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa",
                "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23",
                true
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_webcrypto_secret_key_roundtrips() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // HMAC sign/verify, AES-GCM and AES-CBC encrypt/decrypt roundtrips, and
        // PBKDF2/HKDF derivation all work through the RustCrypto ops (the old
        // stubs returned fake data).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const dec = new TextDecoder();

            // HMAC sign/verify (RFC 4231 key/data).
            const hk = await crypto.subtle.importKey('raw', enc.encode('key'), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
            const sig = await crypto.subtle.sign('HMAC', hk, enc.encode('The quick brown fox jumps over the lazy dog'));
            const sigHex = Array.from(new Uint8Array(sig)).map(b => b.toString(16).padStart(2, '0')).join('');
            const verifyOk = await crypto.subtle.verify('HMAC', hk, sig, enc.encode('The quick brown fox jumps over the lazy dog'));
            const verifyBad = await crypto.subtle.verify('HMAC', hk, sig, enc.encode('tampered'));

            // AES-GCM roundtrip.
            const gk = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt', 'decrypt']);
            const giv = crypto.getRandomValues(new Uint8Array(12));
            const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: giv }, gk, enc.encode('hello gcm'));
            const pt = dec.decode(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: giv }, gk, ct));

            // AES-CBC roundtrip.
            const ck = await crypto.subtle.generateKey({ name: 'AES-CBC', length: 128 }, true, ['encrypt', 'decrypt']);
            const civ = crypto.getRandomValues(new Uint8Array(16));
            const cct = await crypto.subtle.encrypt({ name: 'AES-CBC', iv: civ }, ck, enc.encode('hello cbc'));
            const cpt = dec.decode(await crypto.subtle.decrypt({ name: 'AES-CBC', iv: civ }, ck, cct));

            // PBKDF2 derivation (RFC 6070 vector: PBKDF2-HMAC-SHA256, 1 iter).
            const pk = await crypto.subtle.importKey('raw', enc.encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']);
            const dk = await crypto.subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: enc.encode('salt'), iterations: 1 }, pk, 256);
            const dkHex = Array.from(new Uint8Array(dk)).map(b => b.toString(16).padStart(2, '0')).join('');

            return [sigHex, verifyOk, !verifyBad, pt, cpt, dkHex];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        // RFC 4231 HMAC-SHA-256("key", "The quick brown fox...") =
        //   f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
        // RFC 6070 PBKDF2-HMAC-SHA256("password", "salt", 1, 32) =
        //   120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
                true, true, "hello gcm", "hello cbc",
                "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_webcrypto_pbkdf2_rejects_excessive_iterations() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // A page asking for 2^32 iterations must not pin the single-threaded
        // runtime; the op rejects it with OperationError (upstream cfda91b).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const pk = await crypto.subtle.importKey('raw', enc.encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']);
            try {
                await crypto.subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: enc.encode('salt'), iterations: 4294967295 }, pk, 256);
                return 'no-throw';
            } catch (e) {
                return e.name;
            }
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("OperationError"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_structured_clone_preserves_cryptokey_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // A CryptoKey reached twice in a graph must clone to one shared object
        // that crypto.subtle still accepts (upstream 8698afc + a921668).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const key = await crypto.subtle.importKey('raw', enc.encode('k'), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
            const c = structuredClone({ a: key, b: key });
            const sameObject = c.a === c.b;
            // The clone stays usable by crypto.subtle (key material re-registered).
            const sig = await crypto.subtle.sign('HMAC', c.a, enc.encode('msg'));
            return [sameObject, sig instanceof ArrayBuffer, c.a instanceof CryptoKey];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([true, true, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_get_random_values_and_uuid_from_csprng() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // getRandomValues fills integer typed arrays, randomUUID returns a v4
        // UUID shape, and both reject/fill sensibly.
        let script = r#"() => {
            const u8 = new Uint8Array(32);
            crypto.getRandomValues(u8);
            const nonZero = u8.some(b => b !== 0);
            const uuid = crypto.randomUUID();
            const uuidOk = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid);
            let typeErr = false;
            try { crypto.getRandomValues(new Float64Array(4)); } catch (e) { typeErr = e.name === 'TypeMismatchError'; }
            return [nonZero, uuidOk, typeErr];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, false).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([true, true, true]));
    }

    #[test]
    fn test_node_iterator_returns_root_and_has_detach() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // createNodeIterator was an alias of createTreeWalker, so the first
        // nextNode() silently skipped the root and detach was missing (#467).
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a></a>';
            const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
            const tags = [];
            let n;
            while ((n = it.nextNode())) tags.push(n.tagName);
            return [tags, typeof it.detach, it.root === root, it.referenceNode.tagName, it.pointerBeforeReferenceNode];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([["DIV", "A"], "function", true, "A", false])
        );
    }

    #[test]
    fn test_treewalker_next_document_order_and_reject_prunes() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Document-order forward walk (#432); FILTER_REJECT prunes the whole
        // subtree (#461) rather than just skipping the node.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a><c></c>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'A' ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT; }
            });
            const tags = [];
            let n;
            while ((n = w.nextNode())) tags.push(n.tagName);
            return tags;
        "#).unwrap();
        // A is rejected, so its child B is pruned too; C still follows. Root
        // (DIV) is never returned by a TreeWalker's nextNode.
        assert_eq!(result, serde_json::json!(["C"]));
    }

    #[test]
    fn test_treewalker_skip_descends_into_children() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // FILTER_SKIP must still expose a skipped node's children (#469); the
        // old firstChild stepped straight to the next sibling and returned null.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<section><a></a></section>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'SECTION' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            const first = w.firstChild();
            return first ? first.tagName : null;
        "#).unwrap();
        assert_eq!(result, serde_json::json!("A"));
    }

    #[test]
    fn test_treewalker_previousnode_reverse_order() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // previousNode walked reverse document order and died mid-tree when a
        // candidate was filtered (#462). Walk forward to the end, then back.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a><c></c>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'B' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            while (w.nextNode()) {}
            const tags = [];
            let n;
            while ((n = w.previousNode())) tags.push(n.tagName);
            return tags;
        "#).unwrap();
        // Reverse document order with B skipped: the walk from C finds A, then
        // stops at root (which a backward traversal never returns).
        assert_eq!(result, serde_json::json!(["A"]));
    }

    #[test]
    fn test_treewalker_parentnode_stays_within_root() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // parentNode returned a node OUTSIDE the subtree when currentNode was
        // root, and null instead of root when an accepted ancestor was root
        // itself (#475).
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a>';
            // Skip A, so parentNode must climb past it to the accepted ancestor root.
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'A' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            const b = root.querySelector('b');
            w.currentNode = b;
            const parent = w.parentNode();
            // At root, parentNode must not surface <body> above it.
            w.currentNode = root;
            const above = w.parentNode();
            return [parent === root, above];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, null]));
    }

    #[test]
    fn test_insert_before_flattens_document_fragment_in_order() {
        let mut rt = setup_runtime(r#"<main id="host"><article id="last"></article></main>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const last = document.getElementById('last');
            const fragment = document.createDocumentFragment();
            const first = document.createElement('article');
            const second = document.createElement('article');
            first.id = 'first';
            second.id = 'second';
            fragment.appendChild(first);
            fragment.appendChild(second);

            const returned = host.insertBefore(fragment, last);
            return [
                returned === fragment,
                Array.from(host.children).map(node => node.id),
                fragment.childNodes.length,
                first.parentElement === host,
                second.parentElement === host,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "last"], 0, true, true])
        );
    }

    #[test]
    fn test_replace_child_flattens_document_fragment_and_removes_old_child() {
        let mut rt = setup_runtime(
            r#"<main id="host"><article id="old"></article><article id="tail"></article></main>"#,
        );
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const old = document.getElementById('old');
            const fragment = document.createDocumentFragment();
            const first = document.createElement('article');
            const second = document.createElement('article');
            first.id = 'first';
            second.id = 'second';
            fragment.appendChild(first);
            fragment.appendChild(second);

            const returned = host.replaceChild(fragment, old);
            return [
                returned === old,
                Array.from(host.children).map(node => node.id),
                fragment.childNodes.length,
                old.parentNode === null,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "tail"], 0, true])
        );
    }

    #[test]
    fn test_insert_before_and_replace_child_report_to_mutation_observers() {
        // insertBefore/replaceChild ran the tree mutation but never notified
        // MutationObserver — before()/after()/replaceWith() route through
        // insertBefore, so they were silent too.
        let mut rt = setup_runtime(r#"<main id="host"><p id="a"></p><p id="b"></p></main>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const observer = new MutationObserver(() => {});
            observer.observe(host, { childList: true });
            const x = document.createElement('x-i');
            host.insertBefore(x, document.getElementById('b'));
            const y = document.createElement('x-r');
            host.replaceChild(y, document.getElementById('a'));
            // Observer delivery is a microtask; takeRecords() drains
            // synchronously.
            return observer.takeRecords().map(r => [r.addedNodes.length, r.removedNodes.length]);
        "#).unwrap();
        assert_eq!(result, serde_json::json!([[1, 0], [1, 1]]));
    }

    #[test]
    fn test_checkbox_radio_default_value_on() {
        // A checkbox/radio with no value attribute returns "on" in a real
        // browser, not the empty string; an explicit value attribute wins.
        let mut rt = setup_runtime(
            r#"<input id="cb" type="checkbox"><input id="rd" type="radio"><input id="cbv" type="checkbox" value="yes"><input id="txt" type="text">"#,
        );
        let result = rt.evaluate(r#"
            return [
                document.getElementById('cb').value,
                document.getElementById('rd').value,
                document.getElementById('cbv').value,
                document.getElementById('txt').value,
            ];
        "#).unwrap();
        assert_eq!(result, serde_json::json!(["on", "on", "yes", ""]));
    }

    #[test]
    fn test_child_nodes_is_a_real_nodelist() {
        // childNodes returned a plain Array (Array.isArray true, toString
        // "[object Array]") — an instant fingerprinting tell. A real browser
        // reports "[object NodeList]" and Array.isArray false.
        let mut rt = setup_runtime(r#"<div id="host"><p>A</p><p>B</p></div>"#);
        let result = rt.evaluate(r#"
            const list = document.getElementById('host').childNodes;
            const seen = [];
            list.forEach((n, i) => seen.push([i, n.tagName]));
            return [
                Array.isArray(list),
                Object.prototype.toString.call(list),
                list instanceof NodeList,
                list.length,
                list.item(0).tagName,
                list.item(7),
                [...list].map(n => n.tagName),
                Array.from(list.keys()),
                seen,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                false, "[object NodeList]", true, 2, "P", null,
                ["P", "P"], [0, 1], [[0, "P"], [1, "P"]]
            ])
        );
    }

    #[test]
    fn test_adopt_node_and_toggle_attribute() {
        // Lit/Stencil and several ad SDKs call both; the missing methods threw.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const child = document.createElement('span');
            host.appendChild(child);
            const adopted = document.adoptNode(child);
            const toggles = [
                host.toggleAttribute('hidden'),
                host.toggleAttribute('hidden'),
                host.toggleAttribute('data-x', true),
                host.toggleAttribute('data-x', true),
                host.toggleAttribute('data-x', false),
                host.toggleAttribute('data-x', false),
            ];
            return [
                adopted === child,
                toggles,
                host.hasAttribute('hidden'),
                host.hasAttribute('data-x'),
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, [true, false, true, true, false, false], false, false])
        );
    }

    #[test]
    fn test_clone_node_shallow_preserves_attributes_and_isolation() {
        let mut rt = setup_runtime(
            r#"<section id="src" class="source" data-token="original"><span>child</span></section>"#,
        );
        let result = rt.evaluate(r#"
            const source = document.getElementById('src');
            const clone = source.cloneNode(false);
            clone.className = 'clone';
            source.setAttribute('data-token', 'changed');
            return [
                clone instanceof Element,
                clone.tagName,
                clone.id,
                clone.className,
                clone.getAttribute('data-token'),
                clone.childNodes.length,
                clone.parentNode === null,
                clone !== source,
                source.className,
                source.getAttribute('data-token'),
                source.childNodes.length,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, "SECTION", "src", "clone", "original", 0, true, true,
                "source", "changed", 1
            ])
        );
    }

    #[test]
    fn test_clone_node_deep_keeps_table_children_and_template_contents() {
        // The old innerHTML round-trip parsed through a <div> context, which
        // discards <tr>/<td>/<option> as invalid children. Structural cloning
        // has no parsing context, so they survive; <template> contents hang
        // off a separate fragment and need their own remapped clone.
        let mut rt = setup_runtime(
            r#"<table id="tbl"><tr><td>c1</td></tr></table><template id="tpl"><p>in-template</p></template>"#,
        );
        let result = rt.evaluate(r#"
            const tblClone = document.getElementById('tbl').cloneNode(true);
            const tplClone = document.getElementById('tpl').cloneNode(true);
            return [
                tblClone.querySelectorAll('td').length,
                tblClone.querySelector('td').textContent,
                tplClone.content.childNodes.length,
                tplClone.content.querySelector('p').textContent,
                tplClone.content !== document.getElementById('tpl').content,
            ];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, "c1", 1, "in-template", true]));
    }

    #[test]
    fn test_clone_node_deep_subtree_does_not_overflow() {
        // Structural cloning uses an explicit stack in Rust, so a pathological
        // nesting depth cannot overflow the JS stack.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            let node = document.getElementById('host');
            for (let i = 0; i < 2000; i++) {
                const child = document.createElement('div');
                node.appendChild(child);
                node = child;
            }
            const clone = document.getElementById('host').cloneNode(true);
            let depth = 0, cur = clone;
            while (cur.firstChild) { depth++; cur = cur.firstChild; }
            return [depth];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([2000]));
    }

    #[test]
    fn test_submit_button_click_handler_can_prevent_default_and_navigate() {
        let mut rt = setup_runtime(r#"<form><button type="submit" id="submit">Submit</button></form>"#);
        let href = rt.evaluate(r#"
            const form = document.querySelector('form');
            form.addEventListener('submit', (event) => {
                event.preventDefault();
                location.href = '/submitted';
            });
            document.getElementById('submit').click();
            return location.href;
        "#).unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/submitted"));
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/submitted".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_click_fieldset_disabled_controls_do_not_activate() {
        // <fieldset disabled> disables its descendant controls — no toggle and
        // no click event at all — except descendants of its FIRST <legend>
        // (HTML spec actually-disabled semantics; obscura#721 edge matrix).
        let mut rt = setup_runtime(r#"<form><fieldset disabled>
            <legend><input type=checkbox id=first></legend>
            <legend><input type=checkbox id=second></legend>
            <input type=checkbox id=body>
            </fieldset><input type=checkbox id=outside></form>"#);
        let result = rt.evaluate(r#"
            const hits = [];
            for (const id of ['first','second','body','outside']) {
                const el = document.getElementById(id);
                el.addEventListener('click', () => hits.push(id));
                el.click();
            }
            return [document.getElementById('first').checked,
                    document.getElementById('second').checked,
                    document.getElementById('body').checked,
                    document.getElementById('outside').checked,
                    hits];
        "#).unwrap();
        // First-legend control activates; second-legend and fieldset-body
        // controls are actually-disabled (no toggle, no event); outside is
        // unaffected.
        assert_eq!(
            result,
            serde_json::json!([true, false, false, true, ["first", "outside"]])
        );
    }

    #[test]
    fn test_checkbox_click_clears_indeterminate_and_cancel_restores() {
        // Checkbox activation clears `indeterminate` before the event fires
        // (a listener sees the cleared state); a cancelled click restores
        // both `checked` and `indeterminate`. `indeterminate` is a real IDL
        // property on the prototype, not an expando — `'indeterminate' in el`
        // is true and fresh elements default to false (obscura#721 edge matrix;
        // upstream deliberately skipped this because stock has no property).
        let mut rt = setup_runtime(r#"<input type=checkbox id=plain><input type=checkbox id=cxl><input type=checkbox id=fresh>"#);
        let result = rt.evaluate(r#"
            const plain = document.getElementById('plain'), cxl = document.getElementById('cxl');
            const propReal = ['indeterminate' in plain,
                              Object.getOwnPropertyNames(Object.getPrototypeOf(plain)).includes('indeterminate'),
                              document.getElementById('fresh').indeterminate];
            plain.indeterminate = true;
            let seen = null;
            plain.addEventListener('click', () => { seen = [plain.checked, plain.indeterminate]; });
            plain.click();
            const plainAfter = [plain.checked, plain.indeterminate];
            cxl.indeterminate = true;
            cxl.addEventListener('click', (e) => e.preventDefault());
            cxl.click();
            return [propReal, seen, plainAfter, [cxl.checked, cxl.indeterminate]];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [true, true, false], // real prototype property; defaults false
                [true, false],       // handler observes the flip AND the cleared indeterminate
                [true, false],       // uncancelled click keeps the cleared state
                [false, true],       // cancelled click restores checked AND indeterminate
            ])
        );
    }

    #[test]
    fn test_url_reflection_src_and_href_resolve_absolute() {
        // Next.js/Turbopack webpack runtime does `new URL(x, document.currentScript.src)`
        // to derive its chunk base. If src/href return the raw relative attribute,
        // the base has no scheme and URL construction throws "TypeError: Invalid
        // scheme", so React never hydrates. URL-reflection attributes must return
        // the resolved absolute URL like real browsers.
        let mut rt = setup_runtime(r#"<html><head><script src="/app.js"></script>
            <link rel="stylesheet" href="/style.css"></head><body>
            <img id="logo" src="/logos/x.png"><a id="link" href="/docs">docs</a></body></html>"#);
        let res = rt.evaluate(r#"
            const out = {};
            out.scriptSrc = document.querySelector('script').src;
            out.linkHref = document.querySelector('link').href;
            out.imgSrc = document.getElementById('logo').src;
            out.anchorHref = document.getElementById('link').href;
            out.dataSrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', 'data:image/png;base64,AAA'); return i.src; })();
            out.absSrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', 'https://cdn.example.com/x.png'); return i.src; })();
            out.emptySrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', ''); return i.src; })();
            out.missingSrc = document.createElement('img').src;
            return JSON.stringify(out);
        "#).unwrap();
        let v = serde_json::from_str::<serde_json::Value>(res.as_str().unwrap()).unwrap();
        assert_eq!(v["scriptSrc"], "http://example.com/app.js");
        assert_eq!(v["linkHref"], "http://example.com/style.css");
        assert_eq!(v["imgSrc"], "http://example.com/logos/x.png");
        assert_eq!(v["anchorHref"], "http://example.com/docs");
        assert_eq!(v["dataSrc"], "data:image/png;base64,AAA", "data: URLs stay absolute");
        assert_eq!(v["absSrc"], "https://cdn.example.com/x.png", "absolute stays absolute");
        assert_eq!(v["emptySrc"], "http://example.com/test", "empty src resolves to the document URL");
        assert_eq!(v["missingSrc"], "", "missing src attribute reflects as empty");
    }

    #[test]
    fn test_stealth_fingerprint_apis_pluginarray_and_webgl() {
        // authk.smithery.ai (WorkOS+Cloudflare) crashed after hydration:
        // `ReferenceError: PluginArray is not defined` (bot-detector references
        // the constructor) and `e.uniform2f is not a function` (missing WebGL
        // methods). These must exist and behave like real browsers.
        let mut rt = setup_runtime("<html><body><canvas id='c'></canvas></body></html>");
        let res = rt.evaluate(r#"
            const out = {};
            out.pluginArrayDefined = typeof PluginArray !== 'undefined';
            out.pluginsIsInstance = navigator.plugins instanceof PluginArray;
            out.pluginsLength = navigator.plugins.length;
            out.pluginsIdentity = navigator.plugins === navigator.plugins;
            out.mimeIdentity = navigator.mimeTypes === navigator.mimeTypes;
            out.pluginLength = navigator.plugins[0] && navigator.plugins[0].length;
            out.mimeIsInstance = navigator.mimeTypes instanceof MimeTypeArray;
            const c = document.getElementById('c');
            const gl = c.getContext('webgl');
            out.gl = !!gl;
            out.glIdentity = c.getContext('webgl') === gl;
            out.glInstanceof = gl instanceof WebGLRenderingContext;
            out.gl2Instanceof = document.createElement('canvas').getContext('webgl2') instanceof WebGL2RenderingContext;
            out.glNotThenable = gl.then === undefined;
            out.glSymbolUndefined = gl[Symbol.iterator] === undefined;
            out.uniform2f = typeof gl.uniform2f === 'function';
            out.getContextAttributes = typeof gl.getContextAttributes === 'function' && !!gl.getContextAttributes();
            out.getError = gl.getError() === 0;
            out.unknownMethod = (function(){ try { return typeof gl.someUnknownMethod === 'function' && gl.someUnknownMethod(1,2) === 0; } catch(e) { return 'threw:' + e.message; } })();
            return JSON.stringify(out);
        "#).unwrap();
        let v = serde_json::from_str::<serde_json::Value>(res.as_str().unwrap()).unwrap();
        assert_eq!(v["pluginArrayDefined"], true, "PluginArray global must exist");
        assert_eq!(v["pluginsIsInstance"], true, "navigator.plugins must be a PluginArray");
        assert_eq!(v["pluginsLength"].as_i64().unwrap(), 5);
        assert_eq!(v["pluginsIdentity"], true, "plugins must be a cached singleton (identity is fingerprintable)");
        assert_eq!(v["mimeIdentity"], true, "mimeTypes must be a cached singleton");
        assert_eq!(v["pluginLength"].as_i64().unwrap(), 1, "PDF plugins report one supported mime type");
        assert_eq!(v["mimeIsInstance"], true);
        assert_eq!(v["gl"], true);
        assert_eq!(v["glIdentity"], true, "getContext must return the same context on repeat calls");
        assert_eq!(v["glInstanceof"], true, "gl must be instanceof WebGLRenderingContext");
        assert_eq!(v["gl2Instanceof"], true, "webgl2 context must be instanceof WebGL2RenderingContext");
        assert_eq!(v["glNotThenable"], true, "gl.then must stay undefined or the context becomes thenable");
        assert_eq!(v["glSymbolUndefined"], true, "symbol props must not hit the numNoop fallback");
        assert_eq!(v["uniform2f"], true, "uniform2f must exist");
        assert_eq!(v["getContextAttributes"], true);
        assert_eq!(v["getError"], true);
        assert_eq!(v["unknownMethod"], true, "unknown WebGL methods must not throw");
    }

    #[test]
    fn test_navigator() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let ua = rt.evaluate("navigator.userAgent").unwrap();
        assert!(ua.as_str().unwrap().contains("Chrome"), "UA should contain Chrome: {}", ua);
        let wd = rt.evaluate("navigator.webdriver").unwrap();
        assert_eq!(wd, serde_json::Value::Null);
        let plugins = rt.evaluate("navigator.plugins.length").unwrap();
        assert!(plugins.as_f64().unwrap() > 0.0, "Should have plugins");
        let chrome = rt.evaluate("typeof window.chrome").unwrap();
        assert_eq!(chrome, serde_json::json!("object"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_no_args() {
        let mut rt = setup_runtime("<html><head><title>Test</title></head><body></body></html>");
        let result = rt
            .call_function_on("() => document.title", None, &[], true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("Test Page"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": 10}),
            serde_json::json!({"value": 20}),
        ];
        let result = rt.call_function_on("(a, b) => a + b", None, &args, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 30);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_string_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": "hello"}),
            serde_json::json!({"value": " world"}),
        ];
        let result = rt.call_function_on("(a, b) => a + b", None, &args, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("hello world"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_object_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![serde_json::json!({"value": {"name": "test", "count": 5}})];
        let result = rt
            .call_function_on("(obj) => obj.name + ':' + obj.count", None, &args, true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("test:5"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_return_object() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on("() => ({a: 1, b: 2})", None, &[], true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!({"a": 1, "b": 2}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_object_ref_preserves_methods() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on(
                "() => ({ items: [1,2,3], getLen: function() { return this.items.length; } })",
                None,
                &[],
                false,
            )
            .await.unwrap();
        let oid = result.object_id.unwrap();

        let result2 = rt
            .call_function_on("function() { return this.getLen(); }", Some(&oid), &[], true)
            .await.unwrap();
        assert_eq!(result2.value.unwrap().as_f64().unwrap() as i64, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_node() {
        let mut rt = setup_runtime("<html><body><h1>Hello</h1></body></html>");
        let result = rt
            .evaluate_for_cdp("document.querySelector('h1')", false, false)
            .await.unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.js_type, "object");
        assert!(result.object_id.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_document() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("document", false, false).await.unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.class_name, "HTMLDocument");
    }


    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_resolved_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("Promise.resolve(42)", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_timer_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("new Promise(resolve => setTimeout(() => resolve('done'), 1))", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "done");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_async_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("(async () => 'async-ok')()", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "async-ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_reports_promise_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let err = rt.evaluate_for_cdp("Promise.reject(new Error('boom'))", true, true).await.unwrap_err();
        assert!(err.contains("boom"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_sync_throw() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("(() => { throw new Error('sync-boom') })()", false, false)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: sync-boom");
        assert_eq!(exc.class_name, "Error");
        assert_eq!(outcome.info.subtype.as_deref(), Some("error"));
        assert!(outcome.info.object_id.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_sync_throw_by_value() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("(() => { throw new Error('bv-boom') })()", true, false)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: bv-boom");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_await_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("Promise.reject(new Error('boom'))", true, true)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught (in promise)");
        assert_eq!(exc.description, "Error: boom");
        assert_eq!(exc.class_name, "Error");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_outcome_reports_throw() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .call_function_on_for_cdp_outcome(
                "() => { throw new Error('fn-boom') }",
                None,
                &[],
                false,
                false,
            )
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: fn-boom");
        assert_eq!(exc.class_name, "Error");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_outcome_reports_await_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .call_function_on_for_cdp_outcome(
                "() => Promise.reject(new Error('async-fn-boom'))",
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught (in promise)");
        assert_eq!(exc.description, "Error: async-fn-boom");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_dom_interaction() {
        let mut rt = setup_runtime(r#"<div id="items"><span>A</span><span>B</span></div>"#);
        let args = vec![serde_json::json!({"value": "span"})];
        let result = rt
            .call_function_on(
                "(sel) => document.querySelectorAll(sel).length",
                None,
                &args,
                true,
            )
            .await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 2);
    }

    #[test]
    fn test_inner_html_setter() {
        let mut rt = setup_runtime(r#"<div id="target"><p>Old</p></div>"#);
        rt.execute_script("test", r#"
            var el = document.getElementById('target');
            el.innerHTML = '<strong>Bold</strong><em>Italic</em>';
        "#).unwrap();
        let result = rt.evaluate("document.getElementById('target').innerHTML").unwrap();
        let html = result.as_str().unwrap();
        assert!(html.contains("<strong>"), "innerHTML should contain <strong>, got: {}", html);
        assert!(html.contains("<em>"), "innerHTML should contain <em>, got: {}", html);
        assert!(!html.contains("Old"), "innerHTML should not contain old content, got: {}", html);
    }

    #[test]
    fn test_inner_html_with_nested() {
        let mut rt = setup_runtime(r#"<div id="root"></div>"#);
        rt.execute_script("test", r#"
            var el = document.getElementById('root');
            el.innerHTML = '<ul><li>A</li><li>B</li><li>C</li></ul>';
        "#).unwrap();
        let count = rt.evaluate("document.querySelectorAll('li').length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 3, "Should find 3 li elements after innerHTML set");

        let text = rt.evaluate("document.querySelector('li').textContent").unwrap();
        assert_eq!(text, serde_json::json!("A"));
    }

    #[test]
    fn test_fake_receiver_dom_probe_throws_and_does_not_wipe_document() {
        // Bot detectors probe with a fake receiver:
        //   Object.create(HTMLSelectElement.prototype).setHTMLUnsafe(...)
        // A real browser throws TypeError("Illegal invocation"). Our shim must
        // do the same (this._nid is undefined on the fake object), and must NOT
        // let the undefined nid fall through to Rust as node 0 = document.
        let mut rt = setup_runtime(r#"<div id="target"><p>Survive</p></div>"#);
        let result = rt.evaluate(
            r#"(function() {
                var threw = false, msg = '';
                try {
                    Object.create(HTMLSelectElement.prototype).setHTMLUnsafe('<strong>Wiped</strong>');
                } catch (e) {
                    threw = true; msg = e.name;
                }
                var body = document.getElementById('target');
                return JSON.stringify([threw, msg, document.body.children.length,
                    body ? body.innerHTML : null]);
            })()"#,
        ).unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(arr[0], serde_json::json!(true), "fake-receiver probe should throw");
        assert_eq!(arr[1], serde_json::json!("TypeError"), "should throw TypeError");
        assert!(arr[2].as_u64().unwrap() >= 1, "document body should still have children");
        assert!(arr[3].as_str().unwrap().contains("Survive"), "document content must survive: {}", arr[3]);
    }

    #[test]
    fn test_input_value() {
        let mut rt = setup_runtime(r#"<form><input id="name" type="text" value="initial"><textarea id="bio">old text</textarea></form>"#);
        let val = rt.evaluate("document.getElementById('name').value").unwrap();
        assert_eq!(val, serde_json::json!("initial"));
        rt.execute_script("test", "document.getElementById('name').value = 'new value';").unwrap();
        let val2 = rt.evaluate("document.getElementById('name').value").unwrap();
        assert_eq!(val2, serde_json::json!("new value"));
        let bio = rt.evaluate("document.getElementById('bio').value").unwrap();
        assert_eq!(bio, serde_json::json!("old text"));
    }

    #[test]
    fn test_sequential_runtime_swap() {
        let mut rt1 = setup_runtime("<html><body><h1>Page1</h1></body></html>");
        let title1 = rt1.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(title1, serde_json::json!("Page1"));

        let dom1 = rt1.take_dom();
        drop(rt1);

        let mut rt2 = setup_runtime("<html><body><h1>Page2</h1></body></html>");
        let title2 = rt2.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(title2, serde_json::json!("Page2"));
        drop(rt2);

        if let Some(dom) = dom1 {
            let rt1b = JsRuntime::new();
            rt1b.set_dom(dom);
            rt1b.set_url("http://example.com");
            rt1b.set_title("Page1");
            let mut rt1b = rt1b;
            let title1b = rt1b.evaluate("document.querySelector('h1').textContent").unwrap();
            assert_eq!(title1b, serde_json::json!("Page1"));
        }
    }

    #[test]
    fn test_checkbox_checked() {
        let mut rt = setup_runtime(r#"<input id="cb" type="checkbox" checked>"#);
        let checked = rt.evaluate("document.getElementById('cb').checked").unwrap();
        assert_eq!(checked, serde_json::json!(true));
        rt.execute_script("test", "document.getElementById('cb').checked = false;").unwrap();
        let checked2 = rt.evaluate("document.getElementById('cb').checked").unwrap();
        assert_eq!(checked2, serde_json::json!(false));
    }

    #[test]
    fn test_matches_and_closest() {
        let mut rt = setup_runtime(r#"<div class="outer"><div class="inner"><span id="target">Hi</span></div></div>"#);
        let matches = rt.evaluate("document.getElementById('target').matches('span')").unwrap();
        assert_eq!(matches, serde_json::json!(true));
        let closest = rt.evaluate("document.getElementById('target').closest('.outer').className").unwrap();
        assert_eq!(closest, serde_json::json!("outer"));
        let no_match = rt.evaluate("document.getElementById('target').closest('.nonexistent')").unwrap();
        assert_eq!(no_match, serde_json::Value::Null);
    }

    #[test]
    fn test_clone_node_deep() {
        let mut rt = setup_runtime(r#"<div id="src"><p>A</p><p>B</p></div>"#);
        rt.execute_script("test", r#"
            var src = document.getElementById('src');
            var clone = src.cloneNode(true);
            document.body.appendChild(clone);
        "#).unwrap();
        let count = rt.evaluate("document.querySelectorAll('p').length").unwrap();
        assert!(count.as_f64().unwrap() as i64 >= 4, "Deep clone should duplicate <p> children, got: {}", count);
    }

    #[test]
    fn test_evaluate_multistatement() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("var x = 5; var y = 10; return x + y;").unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 15);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_object_ref_as_argument() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let obj = rt
            .call_function_on("() => ({ x: 42 })", None, &[], false)
            .await.unwrap();
        let oid = obj.object_id.unwrap();

        let args = vec![serde_json::json!({"objectId": oid})];
        let result = rt
            .call_function_on("(obj) => obj.x * 2", None, &args, true)
            .await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 84);
    }

    fn setup_runtime_with_cookies(html: &str) -> (JsRuntime, std::sync::Arc<crate::diting_net::CookieJar>) {
        let dom = crate::diting_dom::parse_html(html);
        let jar = std::sync::Arc::new(crate::diting_net::CookieJar::new());
        let rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt.set_cookie_jar(jar.clone());
        (rt, jar)
    }

    #[test]
    fn test_document_cookie_reads_http_cookies() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("session=abc123; Path=/", &url);
        jar.set_cookie("theme=dark; Path=/", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("session=abc123"), "expected session cookie, got: {}", cookie_str);
        assert!(cookie_str.contains("theme=dark"), "expected theme cookie, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_excludes_httponly() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("visible=yes; Path=/", &url);
        jar.set_cookie("secret=token; Path=/; HttpOnly", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("visible=yes"), "expected visible cookie, got: {}", cookie_str);
        assert!(!cookie_str.contains("secret"), "httpOnly cookie should not be visible to JS, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_setter_stores_in_jar() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.evaluate("document.cookie = 'foo=bar; Path=/'").unwrap();
        let url = url::Url::parse("http://example.com/test").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        assert!(result.as_str().unwrap().contains("foo=bar"));
        let header = jar.get_cookie_header(&url);
        assert!(header.contains("foo=bar"), "cookie should be in jar, got: {}", header);
    }

    #[test]
    fn test_document_cookie_delete_via_max_age() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        rt.evaluate("document.cookie = 'temp=val; Path=/'").unwrap();
        assert!(rt.evaluate("document.cookie").unwrap().as_str().unwrap().contains("temp=val"));
        rt.evaluate("document.cookie = 'temp=; Max-Age=0'").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        assert!(!result.as_str().unwrap().contains("temp="), "cookie should be deleted, got: {}", result);
        assert!(!jar.get_cookie_header(&url).contains("temp="));
    }

    #[test]
    fn test_document_cookie_js_and_http_merge() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("server_sid=xyz; Path=/", &url);
        rt.evaluate("document.cookie = 'client_pref=light'").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("server_sid=xyz"), "expected server cookie, got: {}", cookie_str);
        assert!(cookie_str.contains("client_pref=light"), "expected client cookie, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_empty_when_no_cookies() {
        let (mut rt, _jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let result = rt.evaluate("document.cookie").unwrap();
        assert_eq!(result.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_cookie_no_jar_returns_empty() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("document.cookie").unwrap();
        assert_eq!(result.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_write_appends_to_body() {
        let mut rt = setup_runtime("<html><body><p>Existing</p></body></html>");
        rt.evaluate("document.write('<div>Added</div>')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        let body = html.as_str().unwrap();
        assert!(body.contains("Existing"), "existing content should remain, got: {}", body);
        assert!(body.contains("Added"), "written content should appear, got: {}", body);
    }

    #[test]
    fn test_document_writeln() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.writeln('Hello')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert!(html.as_str().unwrap().contains("Hello"));
    }

    #[test]
    fn test_document_write_multiple_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.write('Hello', ' ', 'World')").unwrap();
        let text = rt.evaluate("document.body.textContent").unwrap();
        assert_eq!(text.as_str().unwrap().trim(), "Hello World");
    }

    #[test]
    fn test_document_open_clears_body() {
        let mut rt = setup_runtime("<html><body><p>Old content</p></body></html>");
        rt.evaluate("document.open()").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert_eq!(html.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_write_html_elements() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate(r#"document.write('<h1 id="title">Test</h1><p>Para</p>')"#).unwrap();
        let h1 = rt.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(h1.as_str().unwrap(), "Test");
        let p = rt.evaluate("document.querySelector('p').textContent").unwrap();
        assert_eq!(p.as_str().unwrap(), "Para");
    }

    #[test]
    fn test_url_relative_resolution() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("new URL('data.json', 'http://example.com/path/page.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/path/data.json");

        let result = rt.evaluate("new URL('/api/data', 'http://example.com/path/page.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/api/data");

        let result = rt.evaluate("new URL('https://other.com/foo', 'http://example.com/bar').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "https://other.com/foo");

        let result = rt.evaluate("new URL('sub/file.js', 'http://example.com/a/b/c.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/a/b/sub/file.js");

        let result = rt.evaluate("new URL('api.json', 'http://localhost:8080/dir/index.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://localhost:8080/dir/api.json");
    }

    // One stream per document. The tokenizer carries its state across the calls.
    // https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-document-write
    #[test]
    fn document_write_joins_an_element_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<di');
                document.write('v id="split">');
                document.write('content</div>');
                const el = document.getElementById('split');
                return el ? el.textContent : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("content"));
    }

    #[test]
    fn document_write_joins_a_tag_name_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<spa');
                document.write('n id="half">x</span>');
                const el = document.getElementById('half');
                return el ? el.tagName : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("SPAN"));
    }

    // The shape the UI5 cachebuster writes: "<script", one per attribute, then ">".
    #[test]
    fn document_write_runs_a_script_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__splitScriptRan = false;
                document.write('<scr' + 'ipt');
                document.write(' id="split-script"');
                document.write('>');
                document.write('globalThis.__splitScriptRan = true;');
                document.write('<\/scr' + 'ipt>');
                return [!!document.getElementById('split-script'), globalThis.__splitScriptRan];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    // A script in the <head> inserts behind itself, so that what it writes runs before what
    // the parser saw after it.
    #[test]
    fn document_write_inserts_at_the_writing_scripts_position() {
        let mut rt = setup_runtime(
            r#"<html><head><script id="writer"></script></head><body><p id="existing">x</p></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                // What the production path sets while a script runs; bootstrap.js
                // assigns __currentScriptNid around every script it prepares.
                globalThis.__currentScriptNid = document.getElementById('writer')._nid;
                document.write('<span id="written"></span>');
                return JSON.stringify({
                  head: Array.from(document.head.children).map(e => e.id || e.tagName),
                  body: Array.from(document.body.children).map(e => e.id || e.tagName),
                });
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(r#"{"head":["writer","written"],"body":["existing"]}"#)
        );
    }

    // Holding back until the close would lose everything written after it. It belongs inside.
    #[test]
    fn document_write_shows_an_element_that_is_never_closed() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<div id="unclosed">hello');
                const el = document.getElementById('unclosed');
                return el ? el.textContent : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn document_write_grows_an_open_element_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<div id="wrap">');
                document.write('<span id="inner">y</span>');
                const inner = document.getElementById('inner');
                return JSON.stringify({
                  wrap: !!document.getElementById('wrap'),
                  inner: !!inner,
                  nested: !!(inner && inner.parentElement && inner.parentElement.id === 'wrap'),
                });
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(r#"{"wrap":true,"inner":true,"nested":true}"#)
        );
    }

    #[test]
    fn document_write_keeps_call_order_at_the_insertion_point() {
        let mut rt = setup_runtime(
            r#"<html><head><script id="writer"></script></head><body></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__currentScriptNid = document.getElementById('writer')._nid;
                document.write('<span id="one"></span>');
                document.write('<span id="two"></span>');
                return Array.from(document.head.children).map(e => e.id).join(',');
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("writer,one,two"));
    }

    #[test]
    fn document_write_reports_to_mutation_observers() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__seen = [];
                const observer = new MutationObserver((records) => {
                  for (const record of records) {
                    for (const node of record.addedNodes) globalThis.__seen.push(node.nodeName);
                  }
                });
                observer.observe(document.body, { childList: true });
                document.write('<span id="watched">z</span>');
                observer.takeRecords().forEach((record) => {
                  for (const node of record.addedNodes) globalThis.__seen.push(node.nodeName);
                });
                return globalThis.__seen.join(',');
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("SPAN"));
    }

    /// A Continue resolution that rewrites the request URL must pass the same
    /// SSRF gate as the original request and as redirect hops — otherwise a
    /// rewrite to an internal address bypasses validate_fetch_url entirely.
    #[tokio::test(flavor = "current_thread")]
    async fn test_intercept_url_rewrite_is_revalidated_against_ssrf() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let mut rt = setup_runtime("<html><body></body></html>");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.set_intercept_tx(tx);
        rt.set_intercept_enabled(true);

        // Answer every intercepted request with a rewrite to a loopback address.
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.resolver.send(crate::diting_js::ops::InterceptResolution::Continue {
                    url: Some("http://127.0.0.1:9/secret".to_string()),
                    method: None,
                    headers: None,
                    body: None,
                });
            }
        });

        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                try {
                    await fetch("http://example.com/data.json");
                    return "not-blocked";
                } catch (e) {
                    return "blocked:" + (e && e.message);
                }
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        let v = result.value.unwrap();
        assert_eq!(v, serde_json::json!("blocked:net::ERR_FAILED"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_fetch_url_input_decodes_binary_body_base64() {
        // Serves a binary body from a real local server: the bootstrap deletes
        // the `Deno` global (stealth), so the op cannot be monkey-patched from
        // JS. URL-object input resolves against document.URL, and the binary
        // body must reach JS intact via the op's base64 envelope.
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.lines().next().unwrap_or("").to_string();
            let body = [0u8, 97, 115, 109, 1, 0, 0, 0];
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/wasm\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            path_tx.send(path).unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const response = await fetch(new URL("/pkg/app_bg.wasm", document.URL));
                return {
                    status: response.status,
                    bytes: Array.from(new Uint8Array(await response.arrayBuffer())),
                };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "status": 200,
                "bytes": [0, 97, 115, 109, 1, 0, 0, 0],
            })
        );
        let request_line = path_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            request_line.starts_with("GET /pkg/app_bg.wasm "),
            "server should see the resolved URL path, got: {}",
            request_line
        );
    }

    /// Browsers send Origin on every non-GET/HEAD request, including
    /// same-origin POSTs (SolidStart server functions 403 without it).
    /// Regression: we only set Origin cross-origin, so a same-origin POST
    /// reached the wire bare and got rejected.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_same_origin_post_sends_origin_header() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hdr_tx, hdr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let origin_line = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("origin:"))
                .unwrap_or("").to_string();
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            hdr_tx.send(origin_line).unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/submit", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const r = await fetch(new URL("/_serverFn/x", document.URL), {
                    method: "POST",
                    headers: { "_h": "{\"x-tsr-serverfn\":\"true\"}" },
                    body: JSON.stringify({ _d: [["name", "AginxBrowser"]] }),
                });
                return { status: r.status };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!({ "status": 200 }));
        let origin_line = hdr_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            origin_line.to_ascii_lowercase().starts_with("origin: http://127.0.0.1:"),
            "same-origin POST must carry Origin, got: {:?}",
            origin_line
        );
    }

    /// The Fetch standard allows 20 redirect hops and rejects the 21st
    /// (upstream 4b90ec3). A local chain of exactly 20 must arrive; one of 21
    /// must fail.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_follows_twenty_redirects_and_rejects_twenty_one() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        fn chain_server(hops: usize) -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                for _ in 0..=hops {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let step: usize = path
                        .trim_start_matches("/hop")
                        .parse()
                        .unwrap_or(0);
                    let response = if step < hops {
                        format!(
                            "HTTP/1.1 302 Found\r\nlocation: /hop{}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            step + 1
                        )
                    } else {
                        "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                            .to_string()
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            port
        }

        let fetch_status = |port: u16| {
            let rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/", port));
            rt
        };
        let script = r#"async () => {
            try {
                const r = await fetch("/hop0");
                return "status:" + r.status;
            } catch (e) {
                return "error:" + (e && e.message);
            }
        }"#;

        let port20 = chain_server(20);
        let mut rt = fetch_status(port20);
        let ok = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(ok.value.unwrap(), serde_json::json!("status:200"));

        let port21 = chain_server(21);
        let mut rt = fetch_status(port21);
        let err = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(err.value.unwrap(), serde_json::json!("error:net::ERR_FAILED"));

        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// fetch() must serialize FormData (incl. File parts with filename and
    /// Content-Type), Blob, and TypedArray bodies the way a browser does
    /// (upstream 3eb28da / 260c4c0). String(body) used to send "[object Blob]".
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_serializes_formdata_blob_and_typed_bodies() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read headers, then exactly content-length body bytes.
                let mut header_end = None;
                let mut content_len = 0usize;
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 { break; }
                    raw.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            for line in head.lines() {
                                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(end) = header_end {
                        if raw.len() >= end + content_len { break; }
                    }
                }
                req_tx.send(raw).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt.call_function_on_for_cdp(
            r#"async (port) => {
                const out = [];
                const run = async (tag, fn) => { try { out.push(tag + ":" + (await fn()).status); } catch (e) { out.push(tag + "!:" + (e && (e.message || e.name))); } };
                const fd = new FormData();
                fd.append("field", "value");
                fd.append("upload", new File([new Uint8Array([1, 2, 3])], "a.bin", { type: "application/octet-stream" }));
                await run("plain", () => fetch("http://127.0.0.1:" + port + "/plain", { method: "POST", body: "x=1" }));
                await run("fd", () => fetch("http://127.0.0.1:" + port + "/fd", { method: "POST", body: fd }));
                await run("blob", () => fetch("http://127.0.0.1:" + port + "/blob", { method: "POST", body: new Blob(["hello"], { type: "text/plain" }) }));
                await run("typed", () => fetch("http://127.0.0.1:" + port + "/typed", { method: "POST", body: new Uint8Array([65, 66, 67]) }));
                return out.join("|");
            }"#,
            None,
            &[serde_json::json!({ "value": port })],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.value.unwrap(), serde_json::json!("plain:200|fd:200|blob:200|typed:200"));

        let plain_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(plain_raw.ends_with(b"x=1"), "plain body mismatch: {:?}", plain_raw);

        let fd_req = String::from_utf8_lossy(&req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).into_owned();
        assert!(fd_req.contains("content-type: multipart/form-data; boundary="), "missing multipart header: {}", fd_req);
        assert!(fd_req.contains("name=\"field\"\r\n\r\nvalue"), "missing field part: {}", fd_req);
        assert!(fd_req.contains("filename=\"a.bin\""), "missing filename: {}", fd_req);
        assert!(fd_req.contains("application/octet-stream"), "missing part content-type: {}", fd_req);

        let blob_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let blob_req = String::from_utf8_lossy(&blob_raw).into_owned();
        assert!(blob_req.contains("content-type: text/plain"), "missing blob content-type: {}", blob_req);
        assert!(blob_req.ends_with("hello"), "blob body mismatch: {}", blob_req);

        let typed_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(typed_raw.ends_with(b"ABC"), "typed body mismatch: {:?}", typed_raw);
    }

    /// Binary request bodies must arrive byte-exact. The deno `#[string]`
    /// boundary used to UTF-8-encode the Latin-1 binary-string body channel,
    /// corrupting `[0,128,255]` into `[0,194,128,195,191]` (upstream obscura
    /// #716). Bodies are now base64-encoded in the JS shim (ASCII-safe across
    /// that boundary) and decoded in `op_fetch_url`, so non-ASCII bytes survive
    /// intact across the typed-array, Blob and multipart File paths.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_binary_bodies_are_byte_exact() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut header_end = None;
                let mut content_len = 0usize;
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 { break; }
                    raw.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            for line in head.lines() {
                                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(end) = header_end {
                        if raw.len() >= end + content_len { break; }
                    }
                }
                req_tx.send(raw).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt.call_function_on_for_cdp(
            r#"async (port) => {
                const u8 = new Uint8Array([0, 128, 255, 16]);
                await fetch("http://127.0.0.1:" + port + "/typed", { method: "POST", body: u8 });
                await fetch("http://127.0.0.1:" + port + "/blob", { method: "POST", body: new Blob([u8]) });
                const fd = new FormData();
                fd.append("f", new File([u8], "b.bin", { type: "application/octet-stream" }));
                await fetch("http://127.0.0.1:" + port + "/fd", { method: "POST", body: fd });
                return "ok";
            }"#,
            None,
            &[serde_json::json!({ "value": port })],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.value.unwrap(), serde_json::json!("ok"));

        let body_bytes = |raw: &[u8]| -> Vec<u8> {
            let pos = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(0);
            raw[pos..].to_vec()
        };

        let typed = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(body_bytes(&typed), vec![0, 128, 255, 16], "typed array body corrupted: {:?}", &typed[typed.len().saturating_sub(16)..]);

        let blob = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(body_bytes(&blob), vec![0, 128, 255, 16], "blob body corrupted");

        let fd = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let needle = [0u8, 128, 255, 16];
        assert!(
            fd.windows(4).any(|w| w == needle),
            "multipart file part corrupted: {:?}",
            &fd[fd.len().saturating_sub(48)..]
        );
    }

    /// RequestCredentials end-to-end (upstream b744b9b): same-origin (the
    /// default) neither sends nor stores cookies cross-origin; "include" does
    /// both, and a credentialed CORS response without Allow-Credentials +
    /// exact origin is blocked.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_honors_request_credentials_across_origins() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        use std::io::{Read, Write};
        fn read_request(stream: &mut std::net::TcpStream) -> String {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }
        fn cookie_header(req: &str) -> String {
            req.lines()
                .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                .map(|l| l[7..].trim().to_string())
                .unwrap_or_default()
        }

        // Page origin: stores a cookie so the same-origin store path runs.
        let listener_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener_a.accept().unwrap();
            read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nset-cookie: a=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").unwrap();
        });

        // Cross origin B: mirrors CORS for the page origin, sets b=1 each time.
        let listener_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        let (cookie_tx, cookie_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let origin = format!("http://127.0.0.1:{}", port_a);
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener_b.accept() else { return };
                let req = read_request(&mut stream);
                cookie_tx.send(cookie_header(&req)).unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\naccess-control-allow-origin: {}\r\naccess-control-allow-credentials: true\r\nset-cookie: b=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    origin
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        // Cross origin C: wildcard ACAO without Allow-Credentials — fine for
        // non-credentialed, blocked for credentials:include.
        let listener_c = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_c = listener_c.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener_c.accept().unwrap();
            read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").unwrap();
        });

        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port_a));
        let result = rt.call_function_on_for_cdp(
            r#"async (pa, pb, pc) => {
                const A = "http://127.0.0.1:" + pa, B = "http://127.0.0.1:" + pb, C = "http://127.0.0.1:" + pc;
                const out = [];
                await fetch(A + "/seed");
                out.push("r1:" + (await fetch(B + "/x")).status);
                out.push("r2:" + (await fetch(B + "/x", { credentials: "include" })).status);
                out.push("r3:" + (await fetch(B + "/x", { credentials: "include" })).status);
                try {
                    await fetch(C + "/x", { credentials: "include" });
                    out.push("c:ok");
                } catch (e) {
                    out.push("c:" + (e && e.message));
                }
                return out.join("|");
            }"#,
            None,
            &[
                serde_json::json!({ "value": port_a }),
                serde_json::json!({ "value": port_b }),
                serde_json::json!({ "value": port_c }),
            ],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let expected = format!(
            "r1:200|r2:200|r3:200|c:Failed to fetch: CORS error: credentialed request requires Access-Control-Allow-Origin 'http://127.0.0.1:{}' and Access-Control-Allow-Credentials 'true'",
            port_a
        );
        assert_eq!(result.value.unwrap(), serde_json::json!(expected));
        let c1 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let c2 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let c3 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        // Cookies are host-scoped (RFC 6265 ignores the port), so once
        // credentials are allowed, B receives every 127.0.0.1 cookie.
        assert_eq!((c1.as_str(), c2.as_str()), ("", "a=1"));
        assert!(c3.split("; ").any(|c| c == "b=1"), "stored cookie missing: {}", c3);
        let b_url = url::Url::parse(&format!("http://127.0.0.1:{}/", port_b)).unwrap();
        assert!(jar.get_cookie_header(&b_url).split("; ").any(|c| c == "b=1"));
    }

    /// Setting innerHTML on the <html> element parses in the "before head"
    /// insertion mode, which synthesizes head and body. The importer must keep
    /// both; it previously returned the synthesized body and dropped the head
    /// (so a <title>/<meta> assigned this way vanished).
    #[test]
    fn documentelement_inner_html_keeps_head_and_body() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){ document.documentElement.innerHTML = '<head><title>T</title></head><body><p>hi</p></body>'; \
                 var t = document.querySelector('title'); var p = document.querySelector('p'); \
                 return (t ? t.textContent : 'no-title') + '|' + (p ? p.textContent : 'no-p'); })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("T|hi"));
    }

    /// Regression guard: innerHTML on an ordinary element still imports the
    /// parsed nodes directly (no head/body is synthesized for a div context),
    /// so the fix above must not change the common case.
    #[test]
    fn ordinary_element_inner_html_imports_content_directly() {
        let mut rt = setup_runtime("<html><body><div id=\"d\"></div></body></html>");
        let v = rt
            .evaluate(
                "(function(){ var d=document.getElementById('d'); d.innerHTML='<span>a</span><span>b</span>'; \
                 return d.children.length + '|' + d.textContent; })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("2|ab"));
    }

    #[test]
    fn insert_adjacent_html_keeps_leading_comments_in_table_contexts() {
        let mut rt = setup_runtime(
            r#"<html><body><table><tbody id="tb"><tr id="row"></tr></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<!--m--><tr><td>v</td></tr>');var row=document.getElementById('row');row.insertAdjacentHTML('beforeend','<!--n--><td>x</td>');return Array.from(tb.childNodes).map(function(n){return n.nodeName}).join('|')+';'+Array.from(row.childNodes).map(function(n){return n.nodeName}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("TR|#comment|TR;#comment|TD"));
    }

    #[test]
    fn insert_adjacent_html_uses_the_insertion_element_as_context() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="d"></div><table id="table"><tbody id="tb"></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.insertAdjacentHTML('beforeend','<tr><td>v</td></tr>');var table=document.getElementById('table');table.insertAdjacentHTML('beforeend','<tr><td>x</td></tr>');var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<tr><td>y</td></tr>tail');return d.firstChild.nodeName+':'+d.textContent+';'+table.lastElementChild.tagName+';'+Array.from(tb.childNodes).map(function(n){return n.nodeName+(n.data?':'+n.data:'')}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("#text:v;TBODY;TR|#text:tail"));
    }

    /// tmp.childNodes is a LIVE list: indexing it while moving nodes into the
    /// document skips every other node. Regression guard for the firstChild-pop
    /// loop in insertAdjacentHTML.
    #[test]
    fn insert_adjacent_html_moves_all_sibling_nodes() {
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.insertAdjacentHTML('beforeend','<span>a</span><span>b</span><span>c</span><span>d</span>');return d.children.length+'|'+d.textContent;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("4|abcd"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_response_array_buffer_preserves_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const bytes = new Uint8Array([9, 0, 97, 115, 109, 1, 8]);
                const response = new Response(bytes.subarray(1, 6));
                return Array.from(new Uint8Array(await response.arrayBuffer()));
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        assert_eq!(result.value.unwrap(), serde_json::json!([0, 97, 115, 109, 1]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wasm_instantiate_streaming_uses_response_array_buffer() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
                const result = await WebAssembly.instantiateStreaming(
                    Promise.resolve(new Response(bytes)),
                    {},
                );
                return result.instance instanceof WebAssembly.Instance;
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        assert_eq!(result.value.unwrap(), serde_json::json!(true));
    }

    #[test]
    fn test_text_decoder_respects_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(
            "new TextDecoder().decode(new Uint8Array([65, 66, 67]).subarray(1, 2))"
        ).unwrap();
        assert_eq!(result.as_str().unwrap(), "B");
    }

    #[test]
    fn test_document_doctype() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><body></body></html>");
        let result = rt.evaluate("document.doctype !== null").unwrap();
        assert_eq!(result, serde_json::json!(true));

        let name = rt.evaluate("document.doctype.name").unwrap();
        assert_eq!(name, serde_json::json!("html"));

        let node_type = rt.evaluate("document.doctype.nodeType").unwrap();
        assert_eq!(node_type.as_f64().unwrap() as i64, 10);
    }

    #[test]
    fn test_document_doctype_null_when_missing() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("document.doctype === null").unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_xml_serializer_doctype() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><body></body></html>");
        let result = rt.evaluate(
            "new XMLSerializer().serializeToString(document.doctype)"
        ).unwrap();
        assert_eq!(result.as_str().unwrap(), "<!DOCTYPE html>");
    }

    #[test]
    fn test_xml_serializer_element() {
        let mut rt = setup_runtime(r#"<html><body><div id="x">Hello</div></body></html>"#);
        let result = rt.evaluate(
            "new XMLSerializer().serializeToString(document.getElementById('x'))"
        ).unwrap();
        let html = result.as_str().unwrap();
        assert!(html.contains("<div"));
        assert!(html.contains("Hello"));
    }

    #[test]
    fn test_create_event_custom_event_has_init_method() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate("typeof document.createEvent('CustomEvent').initCustomEvent")
            .unwrap();
        assert_eq!(kind, serde_json::json!("function"));
    }

    #[test]
    fn test_init_custom_event_sets_fields() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "test",
            r#"
            globalThis.__e = document.createEvent('CustomEvent');
            globalThis.__e.initCustomEvent('myevent', true, false, {hello: 'world'});
        "#,
        )
        .unwrap();
        let t = rt.evaluate("globalThis.__e.type").unwrap();
        assert_eq!(t, serde_json::json!("myevent"));
        let b = rt.evaluate("globalThis.__e.bubbles").unwrap();
        assert_eq!(b, serde_json::json!(true));
        let c = rt.evaluate("globalThis.__e.cancelable").unwrap();
        assert_eq!(c, serde_json::json!(false));
        let d = rt.evaluate("globalThis.__e.detail.hello").unwrap();
        assert_eq!(d, serde_json::json!("world"));
    }

    #[test]
    fn test_create_event_returns_correct_class() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let cust = rt
            .evaluate("document.createEvent('CustomEvent') instanceof CustomEvent")
            .unwrap();
        assert_eq!(cust, serde_json::json!(true));
        let mouse = rt
            .evaluate("document.createEvent('MouseEvent') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouse, serde_json::json!(true));
        let mouses = rt
            .evaluate("document.createEvent('MouseEvents') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouses, serde_json::json!(true));
        let kb = rt
            .evaluate("document.createEvent('KeyboardEvent') instanceof KeyboardEvent")
            .unwrap();
        assert_eq!(kb, serde_json::json!(true));
    }

    #[test]
    fn test_create_event_unknown_type_returns_event() {
        // 7e6f403 flipped the contract: unknown interface names now throw
        // NotSupportedError (Chrome behavior) instead of returning a generic
        // Event whose init* methods would all be missing.
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate(
                r#"(() => {
                    try { document.createEvent('NotARealType'); return 'no-throw'; }
                    catch (e) { return e.name; }
                })()"#,
            )
            .unwrap();
        assert_eq!(kind, serde_json::json!("NotSupportedError"));
    }

    #[test]
    fn test_html_to_markdown_headings() {
        let mut rt = setup_runtime("<html><body><h1>Title</h1><h2>Sub</h2><p>Body</p></body></html>");
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("# Title"), "missing H1: {}", md);
        assert!(md.contains("## Sub"), "missing H2: {}", md);
        assert!(md.contains("Body"), "missing paragraph text: {}", md);
    }

    #[test]
    fn test_html_to_markdown_links_and_inline() {
        let mut rt = setup_runtime(
            r#"<html><body><p>Hello <strong>world</strong> <a href="https://x.test/">link</a> <em>em</em></p></body></html>"#,
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("**world**"), "missing strong: {}", md);
        assert!(md.contains("*em*"), "missing em: {}", md);
        assert!(
            md.contains("[link](https://x.test/)"),
            "missing link: {}",
            md
        );
    }

    #[test]
    fn test_html_to_markdown_lists() {
        let mut rt = setup_runtime(
            "<html><body><ul><li>A</li><li>B</li></ul><ol><li>X</li><li>Y</li></ol></body></html>",
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("- A"), "missing unordered A: {}", md);
        assert!(md.contains("- B"), "missing unordered B: {}", md);
        assert!(md.contains("1. X"), "missing ordered X: {}", md);
    }

    #[test]
    fn test_html_to_markdown_skips_script_and_style() {
        let mut rt = setup_runtime(
            "<html><body><p>Text</p><script>alert(1)</script><style>body{color:red}</style></body></html>",
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("Text"), "missing visible text: {}", md);
        assert!(!md.contains("alert"), "leaked script content: {}", md);
        assert!(!md.contains("color:red"), "leaked style content: {}", md);
    }

    #[test]
    fn test_page_content_puppeteer_pattern() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><head></head><body><p>Test</p></body></html>");
        let result = rt.evaluate(
            "(function() { let retVal = ''; if (document.doctype) retVal = new XMLSerializer().serializeToString(document.doctype); if (document.documentElement) retVal += document.documentElement.outerHTML; return retVal; })()"
        ).unwrap();
        let html = result.as_str().unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html>"));
        assert!(html.contains("<p>Test</p>"));
    }

    #[test]
    fn test_element_from_point_is_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt.evaluate("typeof document.elementFromPoint").unwrap();
        assert_eq!(kind, serde_json::json!("function"));
        let kind2 = rt.evaluate("typeof document.elementsFromPoint").unwrap();
        assert_eq!(kind2, serde_json::json!("function"));
    }

    #[test]
    fn test_element_from_point_in_viewport_returns_body() {
        let mut rt = setup_runtime("<html><body><h1>Hi</h1></body></html>");
        // With diting-layout rects backing getBoundingClientRect, hit testing
        // is real. The h1's UA box (body 8px margin + h1 .67em margin-top,
        // then the 2em line box) starts around y≈30, so (10,40) lands on it;
        // (10,10) sits in the h1's margin — body territory, like Chrome.
        let tag = rt.evaluate("document.elementFromPoint(10, 40)?.tagName").unwrap();
        assert_eq!(tag, serde_json::json!("H1"));
        let margin_area = rt.evaluate("document.elementFromPoint(10, 10)?.tagName").unwrap();
        assert_eq!(margin_area, serde_json::json!("BODY"));
        // Below the h1's line box the point falls through to the body.
        let below = rt.evaluate("document.elementFromPoint(10, 500)?.tagName").unwrap();
        assert_eq!(below, serde_json::json!("BODY"));
    }

    #[test]
    fn test_element_from_point_out_of_viewport_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let neg_x = rt.evaluate("document.elementFromPoint(-1, 10)").unwrap();
        assert_eq!(neg_x, serde_json::Value::Null);
        let neg_y = rt.evaluate("document.elementFromPoint(10, -1)").unwrap();
        assert_eq!(neg_y, serde_json::Value::Null);
        let huge = rt.evaluate("document.elementFromPoint(99999, 99999)").unwrap();
        assert_eq!(huge, serde_json::Value::Null);
    }

    #[test]
    fn test_elements_from_point_returns_array() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let len_in = rt.evaluate("document.elementsFromPoint(10, 10).length").unwrap();
        assert_eq!(len_in.as_f64().unwrap() as i64, 1);
        let len_out = rt.evaluate("document.elementsFromPoint(-1, -1).length").unwrap();
        assert_eq!(len_out.as_f64().unwrap() as i64, 0);
    }

    #[test]
    fn test_element_from_point_non_numeric_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let nan = rt.evaluate("document.elementFromPoint(NaN, 10)").unwrap();
        assert_eq!(nan, serde_json::Value::Null);
        let inf = rt.evaluate("document.elementFromPoint(Infinity, 10)").unwrap();
        assert_eq!(inf, serde_json::Value::Null);
    }

    // Issue #139 — proxy_url must thread through to both the ES-module
    // loader (module_loader.rs) and op_fetch_url's reqwest client
    // (ops.rs::build_request_client). Pre-fix both built clients with
    // `Client::builder().build()` — no proxy — so JS fetch/XHR and
    // dynamic imports silently bypassed BrowserContext.proxy_url.
    //
    // Phase 5.5 RED check: each test references a symbol that does NOT
    // exist on main (proxy_url() accessor, with_proxy ctor,
    // with_base_url_and_proxy ctor), so the tests fail to compile without
    // the prod fix.
    #[test]
    fn http_client_round_trips_proxy_url() {
        use crate::diting_net::{CookieJar, HttpClient};
        let jar = std::sync::Arc::new(CookieJar::new());
        let configured =
            HttpClient::with_options(jar.clone(), Some("http://proxy.test:8080"));
        assert_eq!(
            configured.proxy_url(),
            Some("http://proxy.test:8080"),
            "proxy_url() must expose the value passed to with_options"
        );

        let direct = HttpClient::with_options(jar, None);
        assert_eq!(
            direct.proxy_url(),
            None,
            "proxy_url() must return None when no proxy was configured"
        );
    }

    #[test]
    fn module_loader_stores_proxy_for_dynamic_imports() {
        use crate::diting_js::module_loader::DitingModuleLoader;
        let loader = DitingModuleLoader::with_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
        assert_eq!(loader.proxy_url.as_deref(), Some("http://proxy.test:8080"));
        assert_eq!(loader.base_url, "https://example.com/");

        // Default constructor must keep the historical "no proxy" behaviour.
        let direct = DitingModuleLoader::new("https://example.com/");
        assert_eq!(direct.proxy_url, None);
    }

    #[test]
    fn runtime_with_base_url_and_proxy_constructs_successfully() {
        // Sanity-check the public ctor that page.rs uses to thread proxy
        // through to the module loader. Direct (None) and proxied paths
        // must both initialise the JS environment.
        let _direct = JsRuntime::with_base_url_and_proxy("https://example.com/", None);
        let _proxied = JsRuntime::with_base_url_and_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
    }

    // ── Issue #45 (Playwright actionability) regression tests ────────────────
    // Kept at the end of the module so they don't share textual context with
    // unrelated test additions in other branches (avoids spurious merge
    // conflicts when both this branch and an unrelated bootstrap.js change
    // add tests near the start of `mod tests`).

    /// Playwright >= 1.25 calls `element.checkVisibility(...)` before every
    /// input event. If the method isn't defined Playwright retries until its
    /// action timeout fires. Without a layout engine we can't compute it
    /// properly, so the stub always returns true — still strictly better
    /// than the undefined path.
    #[test]
    fn element_check_visibility_is_callable() {
        let mut rt = setup_runtime(r#"<div id="x">x</div>"#);
        let result = rt
            .evaluate("document.getElementById('x').checkVisibility({checkOpacity: true})")
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let typeof_method = rt
            .evaluate("typeof document.getElementById('x').checkVisibility")
            .unwrap();
        assert_eq!(typeof_method, serde_json::json!("function"));
    }

    /// Playwright's `getByRole` / `getByLabel` locators resolve via ARIA
    /// reflection properties. Without the getters those locators always
    /// fail. Reflect the underlying aria-* attributes.
    #[test]
    fn element_aria_reflection_properties_read_aria_attrs() {
        let mut rt = setup_runtime(
            r#"<button id="b" role="tab" aria-label="Settings" aria-selected="true">x</button>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const el = document.getElementById('b');
                return [el.role, el.ariaLabel, el.ariaSelected];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["tab", "Settings", "true"]));
    }

    /// Setting an ARIA reflection property must write through to the
    /// underlying attribute so frameworks that toggle state via
    /// `el.ariaExpanded = 'true'` actually update the DOM.
    #[test]
    fn element_aria_reflection_setters_write_through() {
        let mut rt = setup_runtime(r#"<div id="d"></div>"#);
        let result = rt
            .evaluate(
                r#"
                const el = document.getElementById('d');
                el.role = 'menu';
                el.ariaExpanded = 'true';
                return [el.getAttribute('role'), el.getAttribute('aria-expanded')];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["menu", "true"]));
    }

    /// Upstream 846ed7d: the Function.prototype.toString override must have a
    /// native function's shape — name, length, non-constructible, no own
    /// `prototype` property.
    #[test]
    fn function_to_string_has_native_function_shape() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const fn = Function.prototype.toString;
                    let constructible = true;
                    try { Reflect.construct(function () {}, [], fn); } catch (e) { constructible = false; }
                    return [fn.toString(), fn.name, fn.length,
                            Object.prototype.hasOwnProperty.call(fn, "prototype"),
                            constructible].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("function toString() { [native code] }|toString|0|false|false")
        );
    }

    /// Upstream 4c33f6d (tamperedFunctions): JS-backed builtins — constructors,
    /// prototype methods, and accessors — must all report [native code].
    #[test]
    fn builtin_members_report_native_code() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const nodeTypeGet = Object.getOwnPropertyDescriptor(Node.prototype, "nodeType").get;
                    return [String(Element), String(Node),
                            String(Element.prototype.getAttribute),
                            String(nodeTypeGet)].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "function Element() { [native code] }|function Node() { [native code] }|function getAttribute() { [native code] }|function get nodeType() { [native code] }"
            )
        );
    }

    /// Upstream 4c33f6d (unusualWindowProperties): internal globals must not
    /// surface through any reflection API on the global object.
    #[test]
    fn internal_globals_are_hidden_from_all_reflection_apis() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const bad = (a) => a.filter(n => typeof n === "string" &&
                        (n[0] === "_" || n.includes("obscura") || n.includes("Obscura") || n.includes("diting") || n.includes("Diting"))).length;
                    const descs = Object.getOwnPropertyDescriptors(window);
                    return [bad(Object.getOwnPropertyNames(window)),
                            bad(Reflect.ownKeys(window)),
                            bad(Object.keys(window)),
                            bad(Object.keys(descs))].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("0|0|0|0"));
    }

    /// Upstream c7e7c70: WebIDL interface globals are non-enumerable in a real
    /// browser (and stay callable).
    #[test]
    fn webidl_interface_globals_are_non_enumerable() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const names = ["Node", "Element", "Document", "Window",
                                   "CSSStyleDeclaration", "DOMStringMap"];
                    const enumerable = names.filter(n => {
                        const d = Object.getOwnPropertyDescriptor(window, n);
                        return !d || d.enumerable !== false;
                    });
                    return [enumerable.length, Object.keys(window).includes("Node"),
                            typeof Node, document.body instanceof Element].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("0|false|function|true"));
    }

    /// Upstream a0e1ba5: CSSStyleDeclaration is a real global interface — the
    /// type of element.style — not merely pre-declared.
    #[test]
    fn cssstyledeclaration_is_a_usable_global_interface() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){var d=Object.getOwnPropertyDescriptor(window,'CSSStyleDeclaration');return (typeof window.CSSStyleDeclaration)+'|'+(document.body.style instanceof CSSStyleDeclaration)+'|'+(d?d.enumerable:'missing');})()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("function|true|false"));
    }

    /// Upstream ec05ed0: dataset is backed by a real DOMStringMap instance
    /// while data-* reflection stays dynamic.
    #[test]
    fn dom_string_map_is_exposed_and_backs_dataset() {
        let mut rt =
            setup_runtime(r#"<html><body><div id="x" data-foo="bar"></div></body></html>"#);
        let v = rt
            .evaluate(
                r#"(() => {
                    const el = document.getElementById("x");
                    const ds = el.dataset;
                    const iface = window.DOMStringMap;
                    const d = Object.getOwnPropertyDescriptor(window, "DOMStringMap");
                    let illegal = false;
                    try { new iface(); } catch (e) { illegal = e instanceof TypeError; }
                    ds.newKey = "1";
                    const reflected = el.getAttribute("data-new-key");
                    delete ds.foo;
                    return [typeof iface, ds instanceof iface,
                            Object.getPrototypeOf(ds) === iface.prototype,
                            ds.constructor === iface,
                            Object.prototype.toString.call(ds),
                            d ? d.enumerable : "missing", illegal, reflected,
                            el.hasAttribute("data-foo"), ds === el.dataset].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "function|true|true|true|[object DOMStringMap]|false|true|1|false|true"
            )
        );
    }

    /// Upstream 9dfc67a: the global's constructor identity is Window, not the
    /// inherited Object — framework environment gates check it directly.
    #[test]
    fn global_window_has_browser_constructor_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(() => [window === self, self.constructor === Window, window instanceof Window, self.document === document, self.navigator === navigator])()",
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!([true, true, true, true, true])
        );
    }

    #[test]
    fn test_style_in_and_object_keys_cssom_parity() {
        // el.style was a bare get/set proxy: `'color' in el.style`,
        // Object.keys(el.style), and camelCase↔dashed sync all failed.
        let mut rt = setup_runtime(r#"<div id="el"></div>"#);
        let result = rt.evaluate(r#"
            const s = document.getElementById('el').style;
            s.fontSize = '20px';
            const keys = Object.keys(s);
            return [
                'color' in s,
                'gap' in s,
                'object-fit' in s,
                s.getPropertyValue('font-size'),
                s.fontSize,
                keys.includes('color'),
                keys.includes('fontSize'),
                s.cssText,
                s.length,
                s.item(0),
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, true, true, "20px", "20px", true, true, "font-size: 20px;", 1, "font-size"
            ])
        );
    }

    #[test]
    fn test_dataset_in_and_object_keys() {
        // `'foo' in el.dataset` and Object.keys(el.dataset) must reflect data-*
        // attributes (CSSOM/DOMStringMap parity).
        let mut rt = setup_runtime(r#"<div id="el" data-foo-bar="1" data-baz="2"></div>"#);
        let result = rt.evaluate(r#"
            const d = document.getElementById('el').dataset;
            return [
                'fooBar' in d,
                'baz' in d,
                'missing' in d,
                Object.keys(d).sort(),
                d.fooBar,
                d.baz,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, true, false, ["baz", "fooBar"], "1", "2"])
        );
    }

    #[test]
    fn test_style_attribute_syncs_both_directions() {
        // CSSStyleDeclaration was in-memory only: parsed inline styles were
        // invisible to el.style.*, and el.style.x = … never reached the
        // attribute or serialization.
        let mut rt = setup_runtime(r#"<div id="el" style="color: red"></div>"#);
        let result = rt.evaluate(r#"
            const el = document.getElementById('el');
            const before = el.style.color;
            el.style.color = 'blue';
            const attrAfterSet = el.getAttribute('style');
            el.setAttribute('style', 'margin: 5px');
            const margin = el.style.margin;
            const colorGone = el.style.color;
            el.style.removeProperty('margin');
            return [before, attrAfterSet, margin, colorGone, el.getAttribute('style')];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["red", "color: blue;", "5px", "", null])
        );
    }

    #[test]
    fn test_insert_adjacent_html_case_insensitive_and_syntax_error() {
        // Position was matched case-sensitively (so 'BeforeEnd' silently
        // no-op'd) and an invalid position didn't throw SyntaxError.
        let mut rt = setup_runtime(r#"<div id="el"><span>child</span></div>"#);
        let result = rt.evaluate(r#"
            const el = document.getElementById('el');
            el.insertAdjacentHTML('BeforeEnd', '<b>X</b>');
            let threw = null;
            try { el.insertAdjacentHTML('sideways', '<i>Y</i>'); } catch (e) { threw = e.name; }
            return [el.innerHTML, threw];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["<span>child</span><b>X</b>", "SyntaxError"])
        );
    }

    #[test]
    fn test_script_runs_once_across_dom_move() {
        // Moving a <script> in the DOM must not execute its inline body a
        // second time (upstream 41a8e1c — "already started" flag).
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            const s = document.createElement('script');
            s.textContent = 'window.__count = (window.__count || 0) + 1;';
            host.appendChild(s);
            const afterFirst = window.__count;
            host.removeChild(s);
            host.appendChild(s);
            const afterMove = window.__count;
            const afterReinsert = (() => { host.removeChild(s); host.appendChild(s); return window.__count; })();
            return [afterFirst, afterMove, afterReinsert];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, 1, 1]));
    }

    #[test]
    fn test_cloned_script_does_not_rerun() {
        // cloneNode of a subtree whose script already ran must not run the
        // clone's script (started state propagates to the clone).
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            const box = document.createElement('div');
            const s = document.createElement('script');
            s.textContent = 'window.__count = (window.__count || 0) + 1;';
            box.appendChild(s);
            host.appendChild(box);
            const afterFirst = window.__count;
            const clone = box.cloneNode(true);
            host.appendChild(clone);
            return [afterFirst, window.__count];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, 1]));
    }

    #[test]
    fn test_innerhtml_script_is_inert() {
        // Scripts created by innerHTML never execute (per spec), unlike direct
        // DOM insertion.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            host.innerHTML = '<script>window.__count = 1;</script>';
            const afterInner = window.__count;
            // A directly-inserted script still runs.
            const s = document.createElement('script');
            s.textContent = 'window.__count = 2;';
            host.appendChild(s);
            return [afterInner, window.__count];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([0, 2]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_data_url_script_executes() {
        // Upstream 0c4740a + f841205: op_fetch_url's HTTP client cannot fetch
        // the data: scheme, so dynamic <script src="data:..."> never ran. The
        // decoder accepts any MIME, %-escapes, fragments, unpadded base64, and
        // non-ASCII via a UTF-8 round-trip — and load fires on every path.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            let loads = 0;
            const mk = (url) => {
                const s = document.createElement('script');
                s.setAttribute('src', url);
                s.addEventListener('load', () => loads++);
                s.addEventListener('error', () => loads -= 100);
                document.body.appendChild(s);
            };
            mk('data:,window.__a=1');
            mk("data:text/plain,window.__g='%C3%A9'");
            mk("data:text/javascript,window.__h='é'");
            mk('data:text/javascript,window.__i=9#frag');
            mk('data:text/javascript;base64,d2luZG93Ll9fYz0z');
            mk('data:text/javascript;base64,d2luZG93Ll9fZD00NA');
            await new Promise(r => setTimeout(r, 20));
            return [window.__a, window.__g, window.__h, window.__i, window.__c, window.__d, loads];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([1, "é", "é", 9, 3, 44, 6])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_data_url_script_invalid_base64_errors() {
        // Upstream f841205: a payload whose length % 4 === 1 can never be
        // valid base64; the decoder must throw instead of executing garbage,
        // and the script element fires error without evaluating anything.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            let errors = 0;
            const mk = (url) => {
                const s = document.createElement('script');
                s.setAttribute('src', url);
                s.addEventListener('error', () => errors++);
                document.body.appendChild(s);
            };
            mk('data:text/javascript;base64,AAAAA');
            mk('data:text/javascript;base64,ab!c');
            mk('data:,window.__ok=1');
            await new Promise(r => setTimeout(r, 20));
            return [errors, window.__ok];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([2, 1]));
    }

    /// Upstream f61493f: the HTML script-fetch algorithm treats an
    /// unsuccessful HTTP response as a network error. A 404 body (here, one
    /// that would clobber a global if it ran) must never become script source.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_script_non_2xx_body_not_evaluated() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = b"window.__leak = 1;";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let script = format!(r#"async () => {{
            let errors = 0, loads = 0;
            const s = document.createElement('script');
            s.setAttribute('src', 'http://127.0.0.1:{port}/missing.js');
            s.addEventListener('error', () => errors++);
            s.addEventListener('load', () => loads++);
            document.body.appendChild(s);
            await new Promise(r => setTimeout(r, 50));
            return [errors, loads, window.__leak === undefined];
        }}"#);
        let result = rt.call_function_on_for_cdp(&script, None, &[], true, true).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!([1, 0, true]));
    }

    /// Upstream a6bb741: a dynamic external script slower than the settle
    /// loop's 500ms fast-path deadline must still be visible as pending while
    /// in flight (so the loop keeps pumping) and must land once its fetch
    /// resolves — including after a failed fetch, where the finally-bracket
    /// must return the counter to zero.
    #[tokio::test(flavor = "current_thread")]
    async fn test_slow_dynamic_script_visible_as_pending_until_lands() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let body = b"window.__slow = 1;";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let insert = format!(r#"
            const s = document.createElement('script');
            s.setAttribute('src', 'http://127.0.0.1:{port}/slow.js');
            document.body.appendChild(s);
        "#);
        rt.evaluate(&insert).unwrap();

        // Pump the event loop past 500ms while the 300ms-slow fetch is in
        // flight; the counter must be observed live at least once, the script
        // must land, and the counter must drain back to zero afterwards.
        let start = tokio::time::Instant::now();
        let mut saw_pending = false;
        while start.elapsed() < std::time::Duration::from_millis(2_000) {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                rt.run_event_loop(),
            ).await;
            if rt.has_pending_dynamic_scripts() {
                saw_pending = true;
            }
            if saw_pending
                && !rt.has_pending_dynamic_scripts()
                && rt.evaluate("window.__slow").unwrap().as_f64() == Some(1.0)
            {
                break;
            }
        }
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert!(saw_pending, "slow dynamic script fetch should be observable as pending");
        assert!(!rt.has_pending_dynamic_scripts(), "counter must drain after the fetch lands");
        assert_eq!(rt.evaluate("window.__slow").unwrap().as_f64(), Some(1.0));
    }

    #[test]
    fn test_domparser_xml_parsererror_on_malformed() {
        // Upstream 53295fa+6927f11+869f700+20c4628: XML mime types get a
        // well-formedness pass; malformed input yields a <parsererror>
        // documentElement that querySelector('parsererror') finds, matching
        // Chrome. Self-closing roots count as complete elements.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const check = (src) => {
                const doc = new DOMParser().parseFromString(src, "application/xml");
                const err = doc.querySelector('parsererror');
                return err ? ('E:' + doc.documentElement.tagName) : ('OK:' + doc.documentElement.tagName);
            };
            return [
                check('<root><a></b></root>'),   // tag mismatch
                check('<root></a></root>'),      // closing tag mismatch
                check('<root/><b/>'),            // extra content after root
                check('<root><a>'),              // unclosed tag
                check('<root><a>1</a></root>'),  // well-formed
                check('<root/>'),                // self-closing root is complete
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "E:PARSERERROR", "E:PARSERERROR", "E:PARSERERROR", "E:PARSERERROR",
                "OK:HTML", "OK:HTML",
            ])
        );
    }

    #[test]
    fn test_domparser_xml_strict_fallback_and_html_unaffected() {
        // The hand-rolled state machine catches what the regex pass cannot
        // (here: zero root elements) and swaps in the generic parsererror.
        // HTML mime types never run either check; comments/CDATA/PI/DOCTYPE
        // are skipped by both layers.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const doc1 = new DOMParser().parseFromString('not xml at all', 'application/xml');
            const textOnly = !!doc1.querySelector('parsererror');
            const doc2 = new DOMParser().parseFromString('<div>hi</div>', 'text/html');
            const htmlOk = !doc2.querySelector('parsererror') && !!doc2.querySelector('div');
            const doc3 = new DOMParser().parseFromString(
                '<?xml version="1.0"?><!-- c --><root><![CDATA[x<y]]></root>', 'application/xml');
            const skipsNoise = !doc3.querySelector('parsererror');
            return [textOnly, htmlOk, skipsNoise];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true, true]));
    }

    #[test]
    fn test_form_submit_bypasses_event_request_submit_fires_it() {
        // Upstream 7e2cabf + ccfa5fb: submit() is a direct pass-through that
        // a submit listener cannot veto; only requestSubmit() (and user
        // clicks) fire the cancelable submit event. requestSubmit's submitter
        // must be a submit button owned by this form.
        let mut rt = setup_runtime(r#"
            <form id="f" action="/go"><input name="q" value="x">
                <button type="submit" id="b">Go</button></form>
            <form id="other"><button type="submit" id="ob">Go</button></form>
            <div id="notabutton"></div>"#);
        // submit(): no event, navigation happens.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            globalThis.__evts = 0;
            form.addEventListener('submit', () => globalThis.__evts++);
            form.submit();
            return [globalThis.__evts];
        "#).unwrap();
        assert_eq!(r, serde_json::json!([0]));
        assert!(rt.take_pending_navigation().is_some(), "submit() must navigate");

        // requestSubmit(): event fires; preventDefault stops navigation.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            form.addEventListener('submit', e => e.preventDefault());
            form.requestSubmit();
            return [globalThis.__evts];
        "#).unwrap();
        assert_eq!(r, serde_json::json!([1]));
        assert!(rt.take_pending_navigation().is_none(), "preventDefault must veto navigation");

        // Submitter validation (ccfa5fb): non-submit-button -> TypeError;
        // foreign submit button -> NotFoundError; valid one fires the event.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            const out = {};
            try { form.requestSubmit(document.getElementById('notabutton')); out.a = 'no-throw'; }
            catch (e) { out.a = e.name; }
            try { form.requestSubmit(document.getElementById('ob')); out.b = 'no-throw'; }
            catch (e) { out.b = e.name; }
            form.requestSubmit(document.getElementById('b'));
            out.c = globalThis.__evts;
            return [out.a, out.b, out.c];
        "#).unwrap();
        // The preventDefault listener from the previous step is still attached,
        // so the valid requestSubmit fires the event (2 total) but does not
        // navigate.
        assert_eq!(r, serde_json::json!(["TypeError", "NotFoundError", 2]));
        assert!(rt.take_pending_navigation().is_none());
    }

    #[test]
    fn test_select_parity_type_selectedindex_add_no_change_on_assign() {
        // Upstream 5308e04: select/textarea report fixed IDL types;
        // a single select implicitly selects its first option (a multiple
        // one idles at -1); programmatic value assignment never fires
        // change (assigning inside a change handler used to loop forever).
        let mut rt = setup_runtime(r#"
            <select id="s"><option value="a">A</option><option value="b">B</option></select>
            <select id="m" multiple><option value="a">A</option></select>
            <textarea id="t"></textarea>"#);
        let result = rt.evaluate(r#"
            const s = document.getElementById('s');
            const m = document.getElementById('m');
            const t = document.getElementById('t');
            let changes = 0;
            s.addEventListener('change', () => changes++);
            s.value = 'b';
            const afterAssign = [changes, s.value, s.selectedIndex];
            s.selectedIndex = 0;
            const afterIndex = [s.value, s.selectedIndex];
            const types = [s.type, m.type, t.type];
            const emptySingle = document.createElement('select');
            const emptyMultiple = document.createElement('select');
            emptyMultiple.setAttribute('multiple', '');
            const opt = document.createElement('option');
            opt.setAttribute('value', 'c'); opt.textContent = 'C';
            s.add(opt);
            return [
                afterAssign, afterIndex, types,
                emptySingle.selectedIndex, emptyMultiple.selectedIndex,
                s.options.length, changes,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [0, "b", 1],          // no change on assignment; selection moved
                ["a", 0],             // selectedIndex setter works both ways
                ["select-one", "select-multiple", "textarea"],
                -1, -1,               // empty selects idle at -1
                3,                    // add() appended the option
                0,                    // assignment never fired change
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_image_is_real_element_and_emulates_load() {
        // Upstream a5a8de7 + 891d850: new Image() must be a real element
        // (style/attribute reflection/event dispatch), assigning .src must
        // emulate a successful decode (complete flips, load fires on both
        // the onload property and listeners), and a pre-defined
        // non-configurable own src (Booking.com instrumentation) must not
        // crash the constructor.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const img = new Image(10, 20);
            const isEl = img instanceof globalThis.HTMLImageElement;
            const styleOk = img.style instanceof globalThis.CSSStyleDeclaration;
            img.style.width = '30px';
            const styleSet = img.style.width === '30px';
            img.width = 10; img.height = 20;
            let viaProp = 0, viaListener = 0;
            img.onload = () => viaProp++;
            img.addEventListener('load', () => viaListener++);
            img.src = '/pixel.png';
            const earlyComplete = img.complete;
            await new Promise(r => setTimeout(r, 20));
            // Anti-bot pattern: hijack createElement and pre-define a
            // non-configurable own src on every <img>.
            const origCreate = document.createElement.bind(document);
            document.createElement = function (tag) {
                const el = origCreate(tag);
                if (String(tag).toLowerCase() === 'img') {
                    Object.defineProperty(el, 'src', { value: '', writable: true, configurable: false });
                }
                return el;
            };
            let hijackSurvived = false, hijackW = 0;
            try {
                const img2 = new Image(7, 8);
                hijackSurvived = true;
                hijackW = img2.width;
            } catch (e) { hijackSurvived = e.message; }
            document.createElement = origCreate;
            return [isEl, styleOk, styleSet, earlyComplete, img.complete,
                    img.naturalWidth, viaProp, viaListener, hijackSurvived, hijackW];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([true, true, true, false, true, 10, 1, 1, true, 7])
        );
    }

    #[test]
    fn test_network_information_event_listeners() {
        // Upstream fc9f524: navigator.connection was a data-only object with
        // no event methods at all; analytics libs calling addEventListener
        // threw. dispatchEvent must run registered listeners with the
        // connection as receiver, honor the on* property, and respect
        // removeEventListener.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const connection = navigator.connection;
            let calls = 0, receiverMatches = false, viaProp = 0;
            function listener(event) {
                calls += 1;
                receiverMatches = this === connection && event.type === 'change';
            }
            connection.addEventListener('change', listener);
            connection.onchange = () => viaProp++;
            const dispatchResult = connection.dispatchEvent(new Event('change'));
            connection.removeEventListener('change', listener);
            connection.dispatchEvent(new Event('change'));
            return [
                typeof connection.addEventListener,
                typeof connection.removeEventListener,
                typeof connection.dispatchEvent,
                dispatchResult, calls, receiverMatches, viaProp,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["function", "function", "function", true, 1, true, 2])
        );
    }

    #[test]
    fn test_document_referrer_semantics() {
        // Upstream edb1785: document.referrer is explicit navigation state —
        // empty for direct automation navigations, the strict-origin-
        // when-cross-origin value for document-initiated hops.
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(rt.evaluate("document.referrer").unwrap(), serde_json::json!(""));
        rt.set_referrer("https://source.example/path?q=1");
        assert_eq!(
            rt.evaluate("document.referrer").unwrap(),
            serde_json::json!("https://source.example/path?q=1")
        );
    }

    #[test]
    fn test_thrown_error_in_one_script_does_not_stop_later_scripts() {
        // Upstream 5c3d560 (regression for #355/#358): an uncaught throw in
        // one inline script must not prevent later independent scripts from
        // running — the babel-polyfill double-load pattern.
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script("s1", "globalThis.__ran1 = true;").unwrap();
        let err = rt
            .execute_script("s2", "throw new Error('only one instance of babel-polyfill is allowed');")
            .unwrap_err();
        assert!(err.contains("babel-polyfill"), "expected the thrown message, got: {}", err);
        rt.execute_script("s3", "globalThis.__ran3 = true;").unwrap();
        let ran = rt
            .evaluate("[globalThis.__ran1 === true, globalThis.__ran3 === true]")
            .unwrap();
        assert_eq!(ran, serde_json::json!([true, true]));
    }

    #[test]
    fn test_event_constructor_webidl_semantics() {
        // Upstream af1e15f: no-arg constructors throw, type coerces to string,
        // CustomEvent.detail defaults to null, createEvent still builds "" type.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const out = [];
            try { new Event(); out.push('no-throw'); } catch (e) { out.push(e.name); }
            try { new CustomEvent(); out.push('no-throw'); } catch (e) { out.push(e.name); }
            out.push(new Event(123).type + ':' + typeof new Event(123).type);
            out.push(String(new CustomEvent('x').detail));
            out.push(String(new CustomEvent('x', { detail: 7 }).detail));
            out.push(new Event('click').type);
            out.push(document.createEvent('Event').type);
            return out.join('|');
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!("TypeError|TypeError|123:string|null|7|click|")
        );
    }

    #[test]
    fn test_promise_rejection_event_requires_promise() {
        // Upstream 0ff1ba0 + 776c915: the promise member is required; the
        // class must exist globally (core-js feature-detects it).
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const promise = Promise.resolve(1);
            const event = new PromiseRejectionEvent('unhandledrejection', { promise, reason: 'failed' });
            let missingThrows = false;
            try { new PromiseRejectionEvent('unhandledrejection'); } catch (e) { missingThrows = e instanceof TypeError; }
            let nullInitThrows = false;
            try { new PromiseRejectionEvent('unhandledrejection', {}); } catch (e) { nullInitThrows = e instanceof TypeError; }
            return [event instanceof Event, event.promise === promise, event.reason, missingThrows, nullInitThrows];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true, "failed", true, true]));
    }

    #[test]
    fn test_storage_event_constructor_and_legacy_factory() {
        // Upstream 776c915: StorageEvent global + legacy createEvent/initStorageEvent path.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const event = new StorageEvent('storage', {
                key: 'theme', oldValue: 'light', newValue: 'dark', url: 'https://example.test/'
            });
            const legacy = document.createEvent('StorageEvent');
            legacy.initStorageEvent('storage', false, false, 'count', '1', '2', 'https://example.test/', null);
            return [
                event instanceof Event,
                event.key, event.oldValue, event.newValue, event.url,
                legacy instanceof StorageEvent, legacy.key, legacy.newValue
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, "theme", "light", "dark", "https://example.test/", true, "count", "2"])
        );
    }

    #[test]
    fn test_create_event_rejects_unknown_and_supports_legacy_aliases() {
        // Upstream 7e6f403: unknown interface names throw NotSupportedError;
        // the DOM Level 2 aliases and hashchange/message map entries resolve.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            let rejected = null;
            try { document.createEvent('NotAnEventInterface'); } catch (e) { rejected = [e.name, e instanceof DOMException]; }
            const aliases = ['Event', 'Events', 'HTMLEvents', 'SVGEvents'].map(name => {
                const event = document.createEvent(name);
                return [event instanceof Event, event.constructor === Event, event.type];
            });
            const hash = document.createEvent('HashChangeEvent') instanceof HashChangeEvent;
            const message = document.createEvent('MessageEvent') instanceof MessageEvent;
            let preRejects = null;
            try { document.createEvent('PromiseRejectionEvent'); } catch (e) { preRejects = e.name; }
            return [rejected, aliases, hash, message, preRejects];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["NotSupportedError", true],
                [[true, true, ""], [true, true, ""], [true, true, ""], [true, true, ""]],
                true, true, "NotSupportedError"
            ])
        );
    }

    #[test]
    fn test_iframe_document_event_listeners() {
        // Upstream 2e3f5d8: addEventListener/removeEventListener/dispatchEvent
        // on an iframe document used to be no-ops.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const iframe = document.createElement('iframe');
            document.body.appendChild(iframe);
            const doc = iframe.contentDocument;
            let calls = 0;
            const listener = () => calls++;
            doc.addEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterRegister = calls;
            doc.addEventListener('probe', listener);
            doc.addEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterDuplicate = calls;
            doc.removeEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterRemove = calls;
            doc.addEventListener('cancelme', e => e.preventDefault());
            const cancelReturn = doc.dispatchEvent(new Event('cancelme', { cancelable: true }));
            const plainReturn = doc.dispatchEvent(new Event('nolisteners'));
            return [!!doc, afterRegister, afterDuplicate, afterRemove, cancelReturn, plainReturn];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, 1, 2, 2, false, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_element_scroll_offsets_and_scroll_event_coalescing() {
        // Upstream 29e20ae + 1c7402d: scrollTop/scrollLeft round-trip, direct
        // assignment fires a scroll event (only on change), and scroll
        // operations coalesce to one event per call.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const el = document.createElement('div');
            document.body.appendChild(el);
            let events = 0;
            el.addEventListener('scroll', () => events++);
            el.scrollTop = 100;          // changed -> 1 event
            el.scrollTop = 100;          // unchanged -> no event
            el.scrollTo(0, 250);         // one coalesced event
            el.scrollBy({ left: 30, top: 50 });
            el.scroll(0, -5);            // clamps both axes back to 0, 1 event
            const offsets = [el.scrollTop, el.scrollLeft];
            await new Promise(r => setTimeout(r, 10));
            return [offsets, events];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([[0, 0], 4]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_window_scroll_moves_page_offset_shared_with_scrolling_element() {
        // Upstream f6ca133: window scroll methods move the page offset stored
        // on the scrolling element; scrollX/scrollY/pageXOffset/pageYOffset are
        // views of it, and a window scroll reaches document AND window listeners.
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let script = r#"async () => {
            const isDocEl = document.scrollingElement === document.documentElement;
            window.scrollTo(0, 500);
            const afterTo = [window.scrollX, window.scrollY];
            window.scrollBy(0, 200);
            const afterBy = [window.pageXOffset, window.pageYOffset];
            window.scrollTo({ left: 10, top: 40 });
            const afterOptions = [window.scrollX, window.scrollY];
            window.scrollTo(0, -100);
            const afterClamp = window.scrollY;
            document.scrollingElement.scrollTop = 90;
            const viaWindow = window.scrollY;
            let win = 0, doc = 0;
            window.addEventListener('scroll', () => win++);
            document.addEventListener('scroll', () => doc++);
            window.scrollBy(0, 400);
            await new Promise(r => setTimeout(r, 10));
            // Five window scroll ops ran in total (four above the listeners
            // plus the final scrollBy); each fires exactly one scroll at the
            // document and one at the window, all drained by the await.
            return [isDocEl, afterTo, afterBy, afterOptions, afterClamp, viaWindow, win, doc, window.scrollY];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([true, [0, 500], [0, 700], [10, 40], 0, 90, 5, 5, 490])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_iframe_load_reaches_onload_and_addeventlistener() {
        // Upstream 2e3f5d8: iframe load used to call el.onload() directly,
        // bypassing addEventListener('load') listeners.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            return await new Promise(resolve => {
                const iframe = document.createElement('iframe');
                const events = [];
                iframe.onload = () => {
                    events.push('property');
                    Promise.resolve().then(() => resolve(events));
                };
                iframe.addEventListener('load', () => events.push('listener'));
                document.body.appendChild(iframe);
                // Unroutable port: fetch rejects, the catch path still fires load.
                iframe.src = 'http://127.0.0.1:1/';
            });
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["property", "listener"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_timer_string_handlers_run_in_global_scope_at_fire_time() {
        // Upstream 452cc85: string setTimeout/setInterval handlers run as
        // global-scope classic scripts at fire time — declarations become
        // globals, and a syntax error surfaces when the timer elapses instead
        // of being swallowed at scheduling. We used to drop string handlers
        // entirely (silent no-op that still returned a timer id).
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            setTimeout('var strVarDecl = 7; window.__strRan = "ran";', 0);
            let scheduleThrew = false;
            try { setTimeout('this is (not javascript', 0); } catch (e) { scheduleThrew = true; }
            window.__intervalCount = 0;
            const iid = setInterval('window.__intervalCount++; clearInterval(window.__iid);', 0);
            window.__iid = iid;
            await new Promise(r => setTimeout(r, 10));
            return [window.__strRan, strVarDecl, scheduleThrew, window.__intervalCount, typeof iid];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["ran", 7, false, 1, "number"])
        );
    }

    #[test]
    fn test_performance_now_is_offset_monotonic_and_bounded() {
        // Upstream cdab919 + d93ff51: now() reports ms since timeOrigin (not
        // the raw epoch), never goes backwards under bursty calls, and does
        // not run ahead of real elapsed time. timeOrigin carries ±50ms of
        // persona jitter, so allow a slightly negative floor.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const n1 = performance.now();
            const offsetSane = n1 > -100 && n1 < 60000;
            let bad = 0, prev = -Infinity;
            for (let i = 0; i < 10000; i++) {
                const t = performance.now();
                if (t < prev) bad++;
                prev = t;
            }
            const lead = performance.now() - (Date.now() - performance.timeOrigin);
            return [offsetSane, bad, lead <= 1];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, 0, true]));
    }

    #[test]
    fn test_location_navigation_coerces_url_objects() {
        // Upstream fe26417: a URL object passed to location.href/assign/replace
        // must coerce to its href string (our _resolveUrl called .startsWith on
        // it and threw).
        let mut rt = setup_runtime("<html><body></body></html>");
        let hrefs = rt.evaluate(r#"
            const before = location.href;
            location.href = new URL('/from-href', before);
            const href = location.href;
            location.assign(new URL('/from-assign', location.href));
            const assigned = location.href;
            location.replace(new URL('/from-replace', location.href));
            return [href, assigned, location.href];
        "#).unwrap();
        assert_eq!(
            hrefs,
            serde_json::json!([
                "http://example.com/from-href",
                "http://example.com/from-assign",
                "http://example.com/from-replace"
            ])
        );
        assert_eq!(
            rt.take_pending_navigation(),
            Some((
                "http://example.com/from-replace".to_string(),
                "GET".to_string(),
                "".to_string()
            ))
        );
    }

    #[test]
    fn test_push_replace_state_without_url_preserves_current_location() {
        // Upstream 1fc5a24: pushState/replaceState with a missing url keep the
        // current document URL — the new history entry must not reset location
        // back to the original document URL.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const first = history.pushState({}, '', '/dashboard');
            const afterReplace = (history.replaceState({scroll:1}), location.pathname);
            history.pushState({}, '', '/a');
            const afterPush = (history.pushState({b:1}), location.pathname);
            return [afterReplace, afterPush];
        "#).unwrap();
        assert_eq!(result, serde_json::json!(["/dashboard", "/a"]));
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_layout_rect_returns_real_geometry_through_get_bounding_client_rect() {
        // Task #108: getBoundingClientRect serves diting-layout geometry, not
        // the synthetic hit-test grid. Two block siblings must land at
        // distinct stacked y positions with full-content width — the grid
        // scatter would give them unrelated (x, y) cells of 100x20.
        let mut rt = setup_runtime(
            "<html><body><div id=\"a\">alpha</div><div id=\"b\">bravo</div></body></html>",
        );
        let result = rt.evaluate(r#"
            const a = document.getElementById("a").getBoundingClientRect();
            const b = document.getElementById("b").getBoundingClientRect();
            return [a.x, a.y, a.width, a.height, b.y > a.y + a.height - 1, b.x === a.x,
                    a.width === innerWidth - 16];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(8), "block x at body's 8px UA content edge");
        assert_eq!(parts[1], serde_json::json!(8), "first block at body content top");
        // Width agrees with the PERSONA viewport minus body's 8px UA margins
        // (set_viewport publishes it to the layout layer; the old hard-coded
        // 1920 broke whenever the persona pool drew a narrower screen).
        assert_eq!(parts[6], serde_json::json!(true), "block spans viewport width minus body margins");
        assert_eq!(parts[4], serde_json::json!(true), "second block stacks below first");
        assert_eq!(parts[5], serde_json::json!(true), "siblings share left edge");
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_layout_rect_cache_invalidates_on_mutation() {
        // A node allocation bumps the tree epoch; the next rect read must
        // reflect the mutated tree (the inserted sibling pushes #b down),
        // not the memoized pre-insert layout.
        let mut rt = setup_runtime(
            "<html><body><div id=\"a\" style=\"height:50px\">a</div><div id=\"b\">b</div></body></html>",
        );
        let before = rt
            .evaluate("document.getElementById('b').getBoundingClientRect().y")
            .unwrap();
        rt.evaluate(
            "const d = document.createElement('div'); d.style.height = '30px'; document.body.insertBefore(d, document.getElementById('b'))",
        )
        .unwrap();
        let after = rt
            .evaluate("document.getElementById('b').getBoundingClientRect().y")
            .unwrap();
        assert_ne!(
            before, after,
            "inserting a 30px block above #b must push it down"
        );
    }

    /// Upstream obscura #704: postMessage's targetOrigin argument must gate
    /// delivery — '*' or a matching origin delivers, a mismatched origin
    /// drops silently (browsers never throw), '/' requires same-origin with
    /// the calling document. The pre-fix wrappers delivered unconditionally,
    /// leaking caller-restricted payloads to whatever frame was targeted.
    #[tokio::test(flavor = "current_thread")]
    async fn test_post_message_target_origin_gates_delivery() {
        let mut rt = setup_runtime(
            "<html><body><iframe src=\"https://frame.example/widget\"></iframe></body></html>",
        );

        // One async eval drives all three gates: mismatched targetOrigin
        // drops silently (iframe origin is frame.example; the caller
        // restricted delivery to trusted.example), matching origin delivers,
        // '*' wildcard delivers.
        let result = rt.evaluate_for_cdp(
            "(async function(){ \
                window.__leak = []; \
                window.addEventListener('message', function(e){ window.__leak.push(e.data) }); \
                const w = document.querySelector('iframe').contentWindow; \
                w.postMessage('secret', 'https://trusted.example'); \
                await new Promise(r => setTimeout(r, 20)); \
                const afterMismatch = window.__leak.slice(); \
                w.postMessage('hello', 'https://frame.example'); \
                await new Promise(r => setTimeout(r, 20)); \
                const afterMatch = window.__leak.slice(); \
                w.postMessage('wild', '*'); \
                await new Promise(r => setTimeout(r, 20)); \
                return [afterMismatch, afterMatch, window.__leak.slice()]; \
            })()",
            true,
            true,
        ).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                [],
                ["hello"],
                ["hello", "wild"],
            ])
        );
    }

    /// Same-origin '/' targetOrigin delivers on a same-origin frame and the
    /// self-targeted window.postMessage(path) honors the same gate.
    #[tokio::test(flavor = "current_thread")]
    async fn test_post_message_same_origin_slash_and_self_gate() {
        let mut rt = setup_runtime(
            "<html><body><iframe src=\"http://example.com/frame\"></iframe></body></html>",
        );
        rt.set_url("http://example.com/test");

        // '/': iframe origin equals page origin → deliver. Self-targeted
        // with mismatched explicit origin → drop. Self-targeted matching
        // origin → deliver.
        rt.evaluate(
            "(function(){ window.__got = []; window.addEventListener('message', function(e){ window.__got.push(e.data) }); document.querySelector('iframe').contentWindow.postMessage('same-origin', '/'); postMessage('self-mismatch', 'https://other.example'); postMessage('self-ok', 'http://example.com'); })()",
        )
        .unwrap();

        let got = rt.evaluate_for_cdp(
            "new Promise(r => setTimeout(() => r(window.__got), 50))",
            true,
            true,
        ).await.unwrap();
        assert_eq!(
            got.value.unwrap(),
            serde_json::json!(["same-origin", "self-ok"])
        );
    }

    /// Upstream obscura #658: relative URL resolution (anchor href, form
    /// action, fetch/XHR input) must resolve against the document BASE url —
    /// the document URL with <base href> folded in — while document.URL
    /// itself stays the plain document URL.
    #[tokio::test(flavor = "current_thread")]
    async fn test_relative_urls_resolve_against_base_href() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let mut rt = setup_runtime(
            "<html><head><base href=\"/assets/\"></head><body>\
             <a id=\"a\" href=\"page.html\">x</a><form id=\"f\" action=\"submit\"></form></body></html>",
        );
        rt.set_url("https://example.com/app/index");

        // Anchor href resolves against /assets/.
        assert_eq!(
            rt.evaluate("document.getElementById('a').href").unwrap(),
            serde_json::json!("https://example.com/assets/page.html")
        );
        // Form action likewise.
        assert_eq!(
            rt.evaluate("document.getElementById('f').action").unwrap(),
            serde_json::json!("https://example.com/assets/submit")
        );
        // fetch() input resolution uses the base as well: a real local server
        // records the path the runtime actually requests.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
            path_tx.send(path).unwrap();
        });
        rt.set_url(&format!("https://example.com/app/index"));
        // Point <base href> at the local server so the resolved fetch lands
        // there (the page URL itself is non-fetchable https).
        rt.evaluate(&format!(
            "document.querySelector('base').setAttribute('href', 'http://127.0.0.1:{}/assets/')", port
        ))
        .unwrap();
        let _ = rt.evaluate_for_cdp(
            "(async function(){ try { await fetch('data.json'); } catch(e) {} })()",
            true,
            true,
        )
        .await;
        let seen = path_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_default();
        assert_eq!(seen, "/assets/data.json");
        // Identity surfaces stay on the plain document URL.
        assert_eq!(
            rt.evaluate("document.URL").unwrap(),
            serde_json::json!("https://example.com/app/index")
        );
    }
}
