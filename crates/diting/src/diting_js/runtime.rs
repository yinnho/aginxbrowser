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

// The V8 termination watchdog family (token, stack-dump sampler, spawn) —
// split from the module root (god-file ratchet). Re-exported so the
// historical `diting_js::runtime::spawn_watchdog` path keeps resolving.
mod watchdog;
pub use watchdog::{spawn_watchdog, WatchdogToken};
use watchdog::watchdog_terminate;

// The eval/callFunctionOn wrapper-source builders (INLINE_EVAL_JS trampoline,
// object slots, meta extraction, the #114 await-settle wrappers).
mod eval_code;

static SNAPSHOT: &[u8] = include_bytes!(env!("AGINXBROWSER_SNAPSHOT_PATH"));

/// Budget for awaiting a script's promise before declaring an eval timeout.
/// The session eval face exposes this per-call (`timeout_ms`); callers that
/// do slow page-side work (uploads through the page's own fetch) pass a
/// larger budget instead of hitting a silent-null (0.4.1 taobao report).
pub const DEFAULT_AWAIT_BUDGET_MS: u64 = 5000;

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
    /// First stack frame, e.g. "at <anonymous>:1:13" — the throw site.
    pub stack_first: Option<String>,
    /// Line/column of the user script frame (the `<anonymous>:L:C` frame),
    /// when one was found — bootstrap eval wrapper frames are skipped so
    /// the position points into the caller's script.
    pub line: Option<u32>,
    pub col: Option<u32>,
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
        // #50 probe: the second captured repro showed tick_fn terminating
        // with NO watchdog fire anywhere near it — this guard is the only
        // other silent terminate_execution() call site. Make it visible.
        let t_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        eprintln!(
            "[heapguard] near-heap-limit tripped at {} bytes (initial {}) — terminating t={t_ms}",
            current_limit, _initial_limit
        );
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
    /// #66: cumulative wall time actually spent inside `poll_event_loop`
    /// across this realm's life (nanoseconds). Pure execution time — a poll
    /// that finds nothing ready registers the reactor bookkeeping only, so
    /// pages parked waiting on timers barely move it. The idle pump diffs
    /// this over a trailing window to catch burners whose every callback
    /// stays under each watchdog budget (they never fire one, so
    /// `watchdog_fired_total` alone cannot see them).
    v8_active_ns: std::cell::Cell<u64>,
    /// #50: set by the watchdog thread at the moment it calls
    /// `terminate_execution()`. `IsolateHandle::is_execution_terminating()`
    /// only reports an *active* termination (one propagating on the stack);
    /// a fire that lands while the session task is parked leaves the flag
    /// pending and invisible until the next V8 entry — by then it kills the
    /// op-delivery tick and drops the batch. `run_event_loop` consults THIS
    /// flag at every poll boundary and cancels the stale termination.
    stale_termination: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Per-module evaluation outcome cache (upstream 4f6d256): browsers
    /// evaluate a module script exactly once per document. deno_core 0.350
    /// asserts on a second mod_evaluate of the same ModuleId instead of
    /// treating it as the spec's module-map no-op, so a page loading the same
    /// module URL twice (duplicate <script type=module src>, or a root already
    /// evaluated earlier as another graph's dependency) panics without this.
    module_evaluations: HashMap<deno_core::ModuleId, Result<(), String>>,
    /// Handle to the ES-module loader this runtime was built with. Kept so
    /// `set_http_client` can reach the loader after construction (the Rc also
    /// lives inside deno_core's RuntimeOptions, which gives nothing back).
    module_loader: Rc<DitingModuleLoader>,
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
        let blob_store = state.borrow().blob_store.clone();

        let module_loader = Rc::new(DitingModuleLoader::with_proxy_and_import_map(
            base_url,
            proxy_url,
            import_map.clone(),
            blob_store,
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
                let kb = crate::env_knobs::js_stack_mb().saturating_sub(2).max(1) * 1024;
                // Returns (); V8 itself logs to stderr if a flag is rejected,
                // which would leave the default ~984 KB ceiling in place.
                v8::V8::set_flags_from_string(&format!("--stack-size={kb}"));
            });
            deno_core::JsRuntime::new(RuntimeOptions {
                extensions: vec![build_extension()],
                module_loader: Some(module_loader.clone()),
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
            v8_active_ns: std::cell::Cell::new(0),
            stale_termination: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            module_evaluations: HashMap::new(),
            module_loader: module_loader.clone(),
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

    /// Re-pin the hardware-persona seed on a freshly booted realm. The realm
    /// is rebuilt per navigation and `__diting_init` self-deletes after
    /// drawing, so cross-page continuity of screen/dpr/GPU/canvas comes from
    /// the Page handing the same seed back every time (see Page::fp_seed).
    pub fn set_fingerprint_seed(&mut self, seed: u64) {
        let s32 = (seed & 0xFFFF_FFFF) as u32;
        let _ = self.runtime.execute_script(
            "<set-fp-seed>",
            format!("globalThis.__diting_setFpSeed({});", s32),
        );
    }
    /// Move only the navigator.language(s) persona (the `__diting_lang`
    /// global behind the bootstrap live getters). Deliberately does NOT
    /// re-pin the ICU default: `set_default_locale` is process-global and
    /// the per-isolate Intl cache is sticky anyway (obscura #734), so a
    /// mid-session re-pin could not take effect and would contaminate
    /// sibling isolates (obscura #778 hazard). Intl locale-argument
    /// binding follows via the bootstrap wrapper reading this same global.
    pub fn set_navigator_language(&mut self, lang: &str) {
        let escaped = lang.replace('\\', "\\\\").replace('\'', "\\'");
        let _ = self.runtime.execute_script(
            "<set-lang>",
            format!("globalThis.__diting_lang = '{}';", escaped),
        );
    }
    /// Move only the navigator.platform persona (the CDP `platform` field of
    /// setUserAgentOverride — per protocol it targets navigator.platform
    /// only, never the UA string, the transport headers, or
    /// userAgentData.platform, which is userAgentMetadata territory).
    pub fn set_navigator_platform(&mut self, platform: &str) {
        let escaped = platform.replace('\\', "\\\\").replace('\'', "\\'");
        let _ = self.runtime.execute_script(
            "<set-platform>",
            format!("globalThis.__diting_platform_override = '{}';", escaped),
        );
    }
    pub fn set_language(&mut self, lang: &str) {
        self.set_navigator_language(lang);
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
            .evaluate_for_cdp_outcome(
                expression,
                return_by_value,
                await_promise,
                DEFAULT_AWAIT_BUDGET_MS,
                None,
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
    ///
    /// `Some(frame_nid)` scopes the eval to that iframe: the single-realm
    /// engine shares one isolate across frames, so the main realm's global
    /// bindings (document/window/self/frames/location) are swapped for the
    /// iframe's content window for the duration of the call and restored on
    /// every return path. Timers firing mid-await observe the frame's
    /// globals (accepted v1 edge); globalThis/navigator/screen stay main.
    pub async fn evaluate_for_cdp_outcome(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
        await_budget_ms: u64,
        frame_nid: Option<u32>,
    ) -> Result<EvalOutcome, String> {
        let swapped = match frame_nid {
            None => false,
            Some(nid) => {
                if !self.frame_swap(nid)? {
                    return Err(format!(
                        "frame eval: iframe nid={} content not ready (detached or cross-origin)",
                        nid
                    ));
                }
                true
            }
        };
        let result = self
            .evaluate_for_cdp_outcome_inner(
                expression,
                return_by_value,
                await_promise,
                await_budget_ms,
            )
            .await;
        if swapped {
            self.frame_restore();
        }
        result
    }

    /// Swap the main realm's global bindings for iframe `nid`'s content
    /// window, returning false when the iframe has no reachable document
    /// (nothing swapped in that case). `document` is an accessor whose
    /// setter switches the active document instance; self/window/frames are
    /// plain data props — assignment is the swap and restore mechanism for
    /// those four. `location` is a non-configurable accessor whose setter
    /// navigates, so it swaps through the `__diting_frame_location` slot its
    /// getter honors (bootstrap.js). The iframe's doc/window are read from
    /// the internal `_iframeDoc`/`_iframeWin` slots, not the public
    /// `contentDocument`/`contentWindow` getters: those apply the web-facing
    /// same-origin gate, while CDP clients are privileged and automate
    /// cross-origin frames as a matter of course (Chrome does the same from
    /// the inspector side).
    fn frame_swap(&mut self, nid: u32) -> Result<bool, String> {
        let code = format!(
            "(function() {{ var el = globalThis._wrap && globalThis._wrap({nid}); var doc = el && (el._iframeDoc || el.contentDocument); var win = el && (el._iframeWin || el.contentWindow); if (!doc || !win) return 'not-ready'; globalThis.__diting_frame_saved = [document, window, self, frames]; document = doc; window = win; self = win; frames = win; Object.defineProperty(globalThis, '__diting_frame_location', {{ value: win.location, configurable: true, writable: true, enumerable: false }}); return 'ok'; }})()",
        );
        let v = self
            .runtime
            .execute_script("<frame-swap>", code)
            .map_err(|e| format!("JS error: {}", e))?;
        Ok(self.v8_to_json(v)?.as_str() == Some("ok"))
    }

    /// Restore the globals stashed by [`frame_swap`]. A restore failure is
    /// logged rather than propagated: the eval result matters more, and the
    /// next navigation's fresh realm clears a stale swap anyway.
    fn frame_restore(&mut self) {
        const RESTORE: &str = "(function() { var s = globalThis.__diting_frame_saved; if (!s) return; document = s[0]; window = s[1]; self = s[2]; frames = s[3]; delete globalThis.__diting_frame_saved; delete globalThis.__diting_frame_location; })()";
        if let Err(e) = self
            .runtime
            .execute_script("<frame-restore>", RESTORE.to_string())
        {
            tracing::warn!("frame restore failed: {}", e);
        }
    }

    async fn evaluate_for_cdp_outcome_inner(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
        await_budget_ms: u64,
    ) -> Result<EvalOutcome, String> {
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let done_counter = self.object_counter;
        // Wrapper semantics (script-not-expression, sentinel protocol, the
        // #114 sync-first await shape) live in eval_code::eval_meta_code.
        let meta_code = Self::eval_meta_code(expression, &oid, done_counter, await_promise);

        // Watchdog-bound: a runaway expression (`while(1){}`) pins the thread
        // inside V8 where the caller's tokio timeouts cannot reach. The floor
        // is 10s, but when the caller asked for a longer budget (#110: page JS
        // legitimately synchronous longer than 10s — e.g. a gBCR that forces
        // the 13s layout of #109) the script must not be beheaded at 10s while
        // its settle half would have waited. A true spin is still terminated:
        // at the caller's own budget, which they chose.
        //
        // NOT execute_script_retry_terminated: that helper clears the flag and
        // retries once, which would just re-enter the spin - the watchdog
        // becomes useless. Pre-clear any stray flag from an earlier watchdog
        // instead, then run the plain one-shot execute.
        self.runtime.v8_isolate().cancel_terminate_execution();
        let eval_wd = self.arm_watchdog(Self::eval_watchdog_duration(await_budget_ms));
        let result = self
            .runtime
            .execute_script("<eval-remote>", meta_code);
        let eval_fired = self.disarm_watchdog(eval_wd);
        if eval_fired {
            let secs = Self::eval_watchdog_duration(await_budget_ms).as_secs();
            let preview: String = expression.chars().take(80).collect();
            tracing::warn!("eval terminated by watchdog (ran >{}s): '{}'", secs, preview);
            return Err(format!("eval timed out: script ran longer than {}s", secs));
        }
        result.map_err(|e| format!("JS error: {}", e))?;

        if await_promise {
            let __t0 = std::time::Instant::now();
            let sentinel = format!("globalThis.__diting_done_{done_counter} === true");
            let settled = self.resolve_promises_until(
                |rt| rt.runtime.execute_script("<done?>", sentinel.clone())
                    .ok()
                    .and_then(|v| rt.v8_to_json(v).ok())
                    .and_then(|j| j.as_bool())
                    .unwrap_or(false),
                await_budget_ms,
            ).await;
            // An unset sentinel means the budget expired with the promise
            // still pending: the result slot was never assigned, so falling
            // through would read `undefined` and surface as a silent
            // `null` — indistinguishable from a genuine null return (the
            // 0.4.1 taobao report's intermittent `result: null` mid
            // batch-upload). Tell the truth instead: the script may still
            // be running, callers must verify side effects before retry.
            if !settled {
                let preview: String = expression.chars().take(80).collect();
                tracing::warn!(
                    "eval await exceeded {}ms budget (still pending): '{}'",
                    await_budget_ms, preview
                );
                return Err(format!(
                    "EVAL_TIMEOUT: expression did not settle within {}ms — the script may still be running; verify side effects before retrying",
                    await_budget_ms
                ));
            }
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
            if self.stored_value_is_undefined(&oid)? {
                Self::undefined_info()
            } else {
                let read = self
                    .runtime
                    .execute_script("<readResult>", Self::object_slot(&oid))
                    .map_err(|e| format!("JS error: {}", e))?;
                let json_val = self.v8_to_json(read)?;
                Self::info_from_json(&json_val)
            }
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
        self.object_store.insert(oid.clone(), Self::object_slot(&oid));
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
                None,
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
        let stack_first = exc_meta
            .get("stack_first")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let line = exc_meta.get("line").and_then(|v| v.as_u64()).map(|v| v as u32);
        let col = exc_meta.get("col").and_then(|v| v.as_u64()).map(|v| v as u32);

        self.object_store.insert(oid.to_string(), Self::object_slot(oid));

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
                stack_first,
                line,
                col,
            }),
        })
    }

    /// Like [`call_function_on_for_cdp`], but a throwing/rejecting function is
    /// reported as an `EvalOutcome` exception instead of being collapsed into
    /// `Err("Promise rejected: …")` or a bare `undefined`.
    /// `frame_nid` scopes the call to an iframe via the same global-binding
    /// swap as [`evaluate_for_cdp_outcome`].
    pub async fn call_function_on_for_cdp_outcome(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
        frame_nid: Option<u32>,
    ) -> Result<EvalOutcome, String> {
        let swapped = match frame_nid {
            None => false,
            Some(nid) => {
                if !self.frame_swap(nid)? {
                    return Err(format!(
                        "frame callFunctionOn: iframe nid={} content not ready (detached or cross-origin)",
                        nid
                    ));
                }
                true
            }
        };
        let result = self
            .call_function_on_for_cdp_outcome_inner(
                function_declaration,
                object_id,
                arguments,
                return_by_value,
                await_promise,
            )
            .await;
        if swapped {
            self.frame_restore();
        }
        result
    }

    async fn call_function_on_for_cdp_outcome_inner(
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
            let code = Self::call_fn_meta_code(
                &setup,
                function_declaration,
                &this_expr,
                &args_list,
                &oid,
                done_counter,
            );

            self.runtime
                .execute_script("<callFnAsync>", code)
                .map_err(|e| format!("JS error: {}", e))?;

            let sentinel = format!("globalThis.__diting_done_{done_counter} === true");
            let settled = self.resolve_promises_until(
                |rt| rt.runtime.execute_script("<done?>", sentinel.clone())
                    .ok()
                    .and_then(|v| rt.v8_to_json(v).ok())
                    .and_then(|j| j.as_bool())
                    .unwrap_or(false),
                DEFAULT_AWAIT_BUDGET_MS,
            ).await;
            // Same contract as evaluate_for_cdp_outcome: an unsettled budget
            // must not fall through to an empty result slot (silent null).
            if !settled {
                return Err(format!(
                    "EVAL_TIMEOUT: callFunctionOn did not settle within {}ms — the function may still be running; verify side effects before retrying",
                    DEFAULT_AWAIT_BUDGET_MS
                ));
            }

            let rejected = self
                .runtime
                .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
                .map_err(|e| format!("JS error: {}", e))?;
            if self.v8_to_json(rejected)?.as_bool().unwrap_or(false) {
                return self.exception_outcome(&oid, true);
            }

            let info = if return_by_value {
                if self.stored_value_is_undefined(&oid)? {
                    Self::undefined_info()
                } else {
                    let read = self
                        .runtime
                        .execute_script("<readResult>", Self::object_slot(&oid))
                        .map_err(|e| format!("JS error: {}", e))?;
                    let json_val = self.v8_to_json(read)?;
                    Self::info_from_json(&json_val)
                }
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
            self.object_store.insert(oid.clone(), Self::object_slot(&oid));
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
                        {slot} = __fn.call(__this, {args});\n\
                        return {slot};\n\
                    }} catch(e) {{\n\
                        {slot} = e;\n\
                        globalThis.__diting_await_meta = {exc_meta_fn};\n\
                        globalThis.__diting_await_rejected = true;\n\
                        return undefined;\n\
                    }}\n\
                }})()",
                setup = setup,
                fn_decl = function_declaration,
                this_expr = this_expr,
                args = args_list,
                slot = Self::object_slot(&oid),
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
            if self.stored_value_is_undefined(&oid)? {
                return Ok(EvalOutcome {
                    info: Self::undefined_info(),
                    exception: None,
                });
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
                    {slot} = __result;\n\
                    globalThis.__diting_await_meta = {meta_fn};\n\
                    globalThis.__diting_await_rejected = false;\n\
                }} catch(e) {{\n\
                    {slot} = e;\n\
                    globalThis.__diting_await_meta = {exc_meta_fn};\n\
                    globalThis.__diting_await_rejected = true;\n\
                }}\n\
            }})()",
            setup = setup,
            fn_decl = function_declaration,
            this_expr = this_expr,
            args = args_list,
            slot = Self::object_slot(&oid),
            meta_fn = Self::meta_extract_js("__result"),
            exc_meta_fn = exc_meta_fn,
        );
        self.runtime
            .execute_script("<callFnRemote>", code)
            .map_err(|e| format!("JS error: {}", e))?;
        let rejected = self
            .runtime
            .execute_script("<readRejected>", "globalThis.__diting_await_rejected".to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        if self.v8_to_json(rejected)?.as_bool().unwrap_or(false) {
            return self.exception_outcome(&oid, false);
        }
        // The IIFE returns nothing (it parks the meta JSON in a global) —
        // read the global back, same as the evaluate handle path does.
        // Reading the IIFE's own return value used to feed `null` into
        // info_from_meta; it went unnoticed because object_id was threaded
        // through unconditionally (obscura#779 tightened that contract).
        let meta = self
            .runtime
            .execute_script("<readMeta>", "globalThis.__diting_await_meta".to_string())
            .map_err(|e| format!("JS error: {}", e))?;
        let meta_str = self.v8_to_json(meta)?;
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str.clone())
        } else {
            meta_str
        };
        self.object_store.insert(oid.clone(), Self::object_slot(&oid));
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
        let code = format!("{} = ({});", Self::object_slot(&oid), js_expression);
        self.runtime
            .execute_script("<store>", code)
            .map_err(|e| format!("Store error: {}", e))?;
        self.object_store.insert(oid.clone(), Self::object_slot(&oid));
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
                {slot} = __result;\n\
                return {meta_fn};\n\
            }})()",
            expr = js_expression,
            slot = Self::object_slot(&oid),
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
        self.object_store.insert(oid.clone(), Self::object_slot(&oid));
        Ok(Self::info_from_meta(&meta_json, Some(oid)))
    }

    #[allow(dead_code)] // CDP Runtime.releaseObject parity
    pub fn release_object(&mut self, object_id: &str) {
        if self.object_store.remove(object_id).is_some() {
            let code = format!("delete {};", Self::object_slot(object_id));
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
        // Backstop for the whole drive, not just the async half. `mod_evaluate`
        // runs the module's top-level code SYNCHRONOUSLY inside the call, and
        // the event-loop poll below runs ready page callbacks — either can pin
        // the thread inside V8, where the tokio budget cannot fire (and neither
        // can the 30s navigation deadline: the current-thread runtime worker
        // itself is blocked, so the HTTP caller gets no answer at all —
        // weixin's appmsg.js did exactly this, hung >2min with no response).
        // Same shape as the post-script settle loop: watchdog 250ms past the
        // budget; disarm cancels/heals a fired termination so the phases after
        // this module still run. Chrome semantics: a killed module ends the
        // module, never the page's load lifecycle.
        let eval_wd = self
            .arm_watchdog(budget + tokio::time::Duration::from_millis(250));
        // deno_core 0.350 panics ("Module already evaluated") rather than
        // treating a second evaluation as the module-map no-op browsers
        // perform; that panic is a success, not a crash.
        let evaluation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.mod_evaluate(module_id)
        }));
        let result = match evaluation {
            Ok(result) => result,
            Err(payload) => {
                let _ = self.disarm_watchdog(eval_wd);
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
        // #50: Self::run_event_loop clears a stale watchdog termination at
        // every poll; both arms map to String so the match below is unchanged.
        let outcome = tokio::time::timeout(budget, async {
            let event_loop = self.run_event_loop();
            tokio::pin!(event_loop);
            tokio::select! {
                biased;
                e = &mut event_loop => {
                    e?;
                    (&mut result).await.map_err(|err| err.to_string())
                }
                r = &mut result => r.map_err(|err| err.to_string()),
            }
        })
        .await;
        let wd_fired = self.disarm_watchdog(eval_wd);
        if wd_fired {
            tracing::warn!(
                "{} evaluation watchdog fired; isolate recovered for later scripts",
                what
            );
        }

        // An eval error or timeout is returned to the page lifecycle. The
        // caller may keep rendering, but must not report the module as loaded.
        let outcome = match outcome {
            Ok(Ok(())) => Ok(()),
            // A fired watchdog makes the eval unwind with "execution
            // terminated" — report the timeout, not the termination symptom.
            Ok(Err(_)) if wd_fired => Err(format!(
                "{} evaluation timed out after {}ms",
                what, budget_ms
            )),
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
                    watchdog_terminate(&isolate_handle);
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
                    // Clear the termination NOW. The isolate was terminated by
                    // a watchdog (usually this guard's own 5s one, possibly a
                    // phase watchdog that fired mid-script) — and V8 keeps
                    // the termination flag pending once the script unwinds.
                    // Left set, every subsequent execution on the isolate
                    // fails instantly: the next scripts "die in 5s" they never
                    // ran, unguarded ones error out, module evals come back
                    // "Uncaught null" (weixin article pages hit all three).
                    // Chrome semantics: a killed script ends the script,
                    // never the page — the page's remaining scripts keep
                    // running. cancel_terminate_execution is the same
                    // healing disarm_watchdog applies at its phase boundary.
                    self.runtime.v8_isolate().cancel_terminate_execution();
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
        // #50: the watchdog armed around an await window can fire while the
        // session task is parked with no JS on the stack (in the captured
        // repro the task was starved 14.65s past the wall-clock deadline).
        // The terminate flag then sits stale on the isolate, and the first
        // V8 entry after wake — usually deno_core's op-delivery tick — dies
        // with "execution terminated", dropping the whole batch of
        // already-popped op results: fetch promises never settle. Detection
        // cannot ask V8: `is_execution_terminating()` only reports an
        // ACTIVE termination (one propagating on the stack) — a fire that
        // landed on a parked isolate leaves the flag pending and reads
        // false. So the watchdog sets our own `stale_termination` flag at
        // the instant of firing; a flag set at a poll boundary is
        // necessarily stale (a genuine synchronous overrun unwinds with Err
        // inside the poll that ran the JS — this heal already ran at the
        // top of that poll). Clear both.
        let isolate = self.isolate_handle.clone();
        let stale = self.stale_termination.clone();
        let active_ns = &self.v8_active_ns;
        let mut inner = std::pin::pin!(self
            .runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default()));
        let result = std::future::poll_fn(|cx| {
            if stale.swap(false, std::sync::atomic::Ordering::SeqCst) {
                isolate.cancel_terminate_execution();
                tracing::warn!("#50: cleared stale V8 termination before event-loop poll");
            }
            // #66: time the poll itself. Ready tasks (timers, promise
            // continuations, message handlers) execute INSIDE this call;
            // a poll that finds nothing ready costs only reactor
            // bookkeeping. Cumulative delta over a trailing window is the
            // honest "how hot is this realm" signal — parked-waiting time
            // doesn't count, so a page polling on a slow timer never trips
            // the busy freeze while an under-budget interval burner does.
            let poll_t0 = std::time::Instant::now();
            let poll_result = std::future::Future::poll(inner.as_mut(), cx);
            active_ns.set(active_ns.get() + poll_t0.elapsed().as_nanos() as u64);
            poll_result
        })
        .await;
        if let Err(e) = &result {
            // An event-loop error (e.g. an exception inside deno_core's
            // __eventLoopTick while resolving op promises) is the silent-loss
            // surface of #50: results already popped from the completed-ops
            // deque are dropped when the tick aborts, and every pump wrapper
            // (run_event_loop_until_idle / _bounded / `let _ =`) discards the
            // Err. Log it here so the loss leaves a signature.
            tracing::warn!("event loop error (op results may be lost): {}", e);
        }
        result.map_err(|e| format!("Event loop error: {}", e))
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
    /// storm that pins the thread is terminated WATCHDOG_HEADROOM_MS past the
    /// budget; a well-behaved page returns as soon as the loop goes idle.
    pub async fn run_event_loop_bounded(&mut self, budget_ms: u64) -> Result<(), String> {
        if budget_ms == 0 {
            return self.run_event_loop().await;
        }
        let budget = std::time::Duration::from_millis(budget_ms);
        let token =
            self.arm_watchdog(budget + std::time::Duration::from_millis(Self::WATCHDOG_HEADROOM_MS));
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
        // Same headroom rationale as run_event_loop_bounded: the idle pump
        // calls this with 200ms slices, and +500ms terminated legitimate
        // multi-second React commits mid-flight (#100). See
        // WATCHDOG_HEADROOM_MS.
        let token =
            self.arm_watchdog(budget + std::time::Duration::from_millis(Self::WATCHDOG_HEADROOM_MS));
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
    pub async fn resolve_promises_until<F>(&mut self, mut done_check: F, max_total_ms: u64) -> bool
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
        // False on deadline/watchdog: the caller must not read a result slot
        // the script never assigned (that's how a timeout used to become a
        // silent `null`).
        let mut settled = false;
        loop {
            if done_check(self) {
                settled = true;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // Pump for a short slice. If the loop returns idle in <tick_ms,
            // run_event_loop returns Ok and we check the predicate again.
            // #50: routed through Self::run_event_loop so each poll first
            // clears a terminate flag the watchdog left on a parked isolate.
            let _ = tokio::time::timeout(
                tokio::time::Duration::from_millis(tick_ms),
                self.run_event_loop(),
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
        settled
    }
    #[cfg_attr(not(test), allow(dead_code))] // used by suspend_js/resume_js lifecycle tests
    pub fn take_dom(&self) -> Option<DomTree> {
        self.state.borrow_mut().dom.take()
    }

    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        let state = self.state.borrow();
        state.dom.as_ref().map(f)
    }

    /// Read access to the shared JS state (scroll offset mirror, image byte
    /// table, viewport) for the band-paint frame paths. The closure must not
    /// re-enter the runtime (same contract as `with_dom`).
    #[cfg(feature = "screenshot")]
    pub fn with_state<R>(&self, f: impl FnOnce(&JsState) -> R) -> R {
        let state = self.state.borrow();
        f(&state)
    }

    /// Mutating variant of [`with_state`]. Field writes that affect layout
    /// (e.g. the image byte table) must also drop `layout_cache` —
    /// `ops::store_image_bytes` already does.
    #[cfg(feature = "screenshot")]
    pub fn with_state_mut<R>(&self, f: impl FnOnce(&mut JsState) -> R) -> R {
        let mut state = self.state.borrow_mut();
        f(&mut state)
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

    #[cfg_attr(not(test), allow(dead_code))] // helper of call_function_on (test-exercised)
    fn resolve_this(&self, object_id: Option<&str>) -> String {
        match object_id {
            Some(oid) => {
                if let Some(retrieval) = self.object_store.get(oid) {
                    retrieval.clone()
                } else if let Some(nid) = oid.strip_prefix("node-").and_then(|s| s.parse::<u64>().ok())
                {
                    format!(
                        "(function() {{ \
                            var nid = {nid}; \
                            var cache = globalThis._cache || new Map(); \
                            if (cache.has(nid)) return cache.get(nid); \
                            return null; \
                        }})()"
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
        let mut setup_lines = vec!["globalThis._namedBoot&&globalThis._namedBoot();".to_string()];
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

    /// Chrome's RemoteObject for JS `undefined` is `{type:"undefined"}` —
    /// no value, no subtype (obscura#779).
    fn undefined_info() -> RemoteObjectInfo {
        RemoteObjectInfo {
            js_type: "undefined".into(),
            subtype: None,
            class_name: String::new(),
            description: String::new(),
            object_id: None,
            value: None,
        }
    }

    /// `v8_to_json` cannot tell a stored `undefined` from a missing value —
    /// both come back as JSON null — so the byValue read-backs ask the
    /// runtime for the stored value's typeof first (obscura#779).
    fn stored_value_is_undefined(&mut self, oid: &str) -> Result<bool, String> {
        let probe = self
            .runtime
            .execute_script(
                "<typeofResult>",
                format!("typeof {} === 'undefined'", Self::object_slot(oid)),
            )
            .map_err(|e| format!("JS error: {}", e))?;
        Ok(self.v8_to_json(probe)?.as_bool().unwrap_or(false))
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

        // Chrome's RemoteObject for a primitive carries the real JSON value
        // (which meta_extract_js put in `value` for number/boolean/string),
        // and a handle (objectId) only for objects and functions — a number
        // or `undefined` result is never something the client can call into
        // (obscura#779).
        let value = match js_type.as_str() {
            "number" | "boolean" | "string" => meta.get("value").cloned(),
            _ => None,
        };
        let object_id = match js_type.as_str() {
            "object" | "function" => object_id,
            _ => None,
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
