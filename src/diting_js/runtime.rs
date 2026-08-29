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

/// deno_core 0.411 captures the ambient tokio handle when the isolate is
/// registered and later spawns V8 platform tasks (the memory reducer's
/// delayed GC probe, ~30s after heap growth, posts from inside an eval)
/// through it — `std::process::abort()` when none was captured, by design,
/// since a panic cannot unwind through V8's FFI frames. Server paths run
/// under `#[tokio::main]`, but sync callers (tests, warmup) don't. This
/// enters a process-lifetime background runtime; the returned guard must
/// outlive the `deno_core::JsRuntime::new` call so registration finds the
/// handle. Delayed tasks keep spawning afterward through the stored
/// `Handle` clone, which stays valid because the runtime never drops.
static BACKGROUND_TOKIO: std::sync::OnceLock<tokio::runtime::Runtime> =
    std::sync::OnceLock::new();

fn enter_tokio_context() -> Option<tokio::runtime::EnterGuard<'static>> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return None;
    }
    let runtime = BACKGROUND_TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("background tokio runtime for V8 platform tasks")
    });
    Some(runtime.enter())
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

        // Must stay alive through the deno_core::JsRuntime::new call inside
        // the lock below — isolate registration reads the ambient handle.
        let _tokio_context = enter_tokio_context();

        // Serialize isolate construction process-wide: V8's JSDispatchTable
        // setup is not safe to run from several threads at once, and sessions
        // plus one-shot ops each construct on their own thread concurrently
        // (upstream obscura hit this under thread-per-connection, #430).
        let mut runtime = {
            let _construct_guard = ISOLATE_CONSTRUCT_LOCK.lock().unwrap();
            // One-shot before the first isolate: raise V8's own JS stack
            // ceiling. The default (~984 KB) is fine for hand-written code,
            // but minified SPA bundles (juejin.cn class) recurse past it and
            // the page dies with `RangeError: Maximum call stack size
            // exceeded` before it renders. The flag is in KB; keep a 2 MB
            // margin under the hosting thread's native stack, which callers
            // size to match (config::js_stack_mb, default 32 MB).
            static V8_STACK_FLAG: std::sync::Once = std::sync::Once::new();
            V8_STACK_FLAG.call_once(|| {
                let kb = crate::config::js_stack_mb().saturating_sub(2).max(1) * 1024;
                // Returns (); V8 itself logs to stderr if a flag is rejected,
                // which would leave the default ~984 KB ceiling in place.
                v8::V8::set_flags_from_string(&format!("--stack-size={kb}"));
            });
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
        // Pin ICU's default locale to the SAME source (obscura#734 lineage):
        // V8's Intl follows the process locale otherwise, so a non-matching
        // LANG leaves Intl.DateTimeFormat().resolvedOptions().locale (and
        // every Intl default) disagreeing with navigator.language and the
        // Accept-Language header the net layer sends - a three-way locale
        // mismatch that's a hard headless tell. Take the first q-weights-
        // stripped BCP-47 tag (the same fold __ditingLangList does for
        // navigator.language).
        let first_tag = lang
            .split(',')
            .next()
            .and_then(|t| t.split(';').next())
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .unwrap_or("zh-CN");
        deno_core::v8::icu::set_default_locale(first_tag);
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

        // Evaluate with Runtime.evaluate semantics: the input is a script,
        // statements are legal, and the completion value of the last
        // statement is the result. Indirect eval (`(0,eval)(literal)`) runs
        // it at global scope — matching Chrome, where
        // `Runtime.evaluate("var x=1; x*2")` returns 2. The old
        // `await (\n{expr}\n)` / `(\n{expr}\n)` expression wrap turned any
        // statement syntax into an uncatchable parse-time SyntaxError
        // (`Unexpected token ';'`), so statement-style probes and client
        // bundles died silently. serde_json::to_string gives a JS-safe
        // string literal, so trailing `//# sourceURL=...` comments stay
        // inside the string instead of eating a paren. A bare `{...}` body
        // is parenthesized so pasted JSON evaluates as an object literal
        // rather than a valueless block.
        let trimmed = expression.trim();
        let body = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            format!("({})", trimmed)
        } else {
            trimmed.to_string()
        };
        let expr_literal = serde_json::to_string(&body).unwrap_or_else(|_| "\"\"".to_string());
        let done_counter = self.object_counter;
        let exc_meta_fn = Self::exception_meta_extract_js("e");
        // Both paths set __diting_await_meta + __diting_await_rejected so the
        // read-back after the IIFE is uniform whether the expression was
        // awaited or run synchronously.
        let meta_code = if await_promise {
            format!(
                "(async function() {{\n\
                    try {{\n\
                        var __result = await __ditingEvalScript({expr});\n\
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
                expr = expr_literal,
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
                        __result = __ditingEvalScript({expr});\n\
                        globalThis.__diting_objects['{oid}'] = __result;\n\
                        globalThis.__diting_await_meta = {meta_fn};\n\
                        globalThis.__diting_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        globalThis.__diting_objects['{oid}'] = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                    }}\n\
                }})()",
                expr = expr_literal,
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
        // CDP Runtime.evaluate semantics: the input is a *script*, not an
        // expression — statements are legal and the completion value of the
        // last statement is the result. Indirect eval (`(0,eval)(...)`) runs
        // it at global scope and hands back the completion value, so
        // `var x=1; x*2` returns 2 exactly like Chrome. The previous
        // `return (...);` expression wrap turned any statement syntax into
        // `Unexpected token ';'` (unless the script happened to start with
        // one of six hard-coded statement keywords), which silently broke
        // clients that evaluate statement scripts — e.g. probing a
        // WAF challenge page with `try { readygo(); } catch(e) {...}`.
        // serde_json::to_string emits a JS-safe string literal (quotes,
        // newlines, U+2028/2029 all escaped), and a trailing
        // `//# sourceURL=...` line comment lives inside the literal instead
        // of eating the closing paren.
        //
        // One convenience divergence from raw script semantics: a bare
        // `{...}` input is parenthesized so pasted JSON evaluates as an
        // object literal (like DevTools console), not as a block whose
        // completion value is undefined.
        let trimmed = expression.trim();
        let body = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            format!("({})", trimmed)
        } else {
            trimmed.to_string()
        };
        let literal = serde_json::to_string(&body).unwrap_or_else(|_| "\"\"".to_string());
        format!(
            "(function() {{ try {{ return __ditingEvalScript({}); }} catch(e) {{ return null; }} }})()",
            literal
        )
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
        // 0.411 removed JsRuntime::handle_scope; deno_core::scope! is the blessed rebuild.
        deno_core::scope!(scope, self.runtime);
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

    /// Chrome spells a JS number via Number→String: integral doubles lose
    /// the fraction ("2", not "2.0"). v8_to_json boxes every JS number as an
    /// f64 serde Number, whose Display keeps float-ness, and JS clients only
    /// normalize that away for `value` (JSON.parse), never for the
    /// `description` string (obscura#541 probe follow-up, same class as the
    /// #576 integer coordinates).
    fn chrome_number_string(n: &serde_json::Number) -> String {
        match n.as_f64() {
            Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < i64::MAX as f64 => {
                format!("{}", f as i64)
            }
            _ => n.to_string(),
        }
    }

    /// Integral f64 numbers serialize without the trailing ".0" so
    /// `returnByValue` payloads match Chrome's wire form for non-JS clients
    /// too (serde_json would print 2.0).
    fn chrome_number_value(n: &serde_json::Number) -> serde_json::Value {
        if let Some(f) = n.as_f64() {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                return serde_json::Value::Number(serde_json::Number::from(f as i64));
            }
        }
        serde_json::Value::Number(n.clone())
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
                description: Self::chrome_number_string(n),
                object_id: None,
                value: Some(Self::chrome_number_value(n)),
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
mod tests;
