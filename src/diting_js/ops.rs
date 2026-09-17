use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::op2;
use deno_core::OpState;
use deno_core::Extension;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crate::diting_dom::{AttachShadowError, DomTree, NodeData, NodeId, ShadowRootMode};
use html5ever::namespace_url;
use crate::diting_net::{CookieJar, HttpClient};
use url::Url;

/// CDP Fetch-domain resolution: what a client answers a paused request with.
/// Produced by the CDP bridge's `Fetch.continueRequest` / `fulfillRequest` /
/// `failRequest` handlers.
#[derive(Debug)]
pub enum InterceptResolution {
    Continue {
        url: Option<String>,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        body: Option<Vec<u8>>,
    },
    Fulfill {
        status: u16,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    },
    Fail { reason: String },
}

/// A paused request surfaced to the interception channel (CDP
/// `Fetch.requestPaused` shape). The resolver answers with an
/// `InterceptResolution`. Field readers are the CDP layer, which mints its
/// own pause ids; ops code only moves the struct through the channel.
pub struct InterceptedRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub resource_type: String,
    pub resolver: tokio::sync::oneshot::Sender<InterceptResolution>,
}

/// How long a paused fetch()/XHR waits for a CDP client's resolution before
/// falling through to the real request (Continue semantics). The CDP bridge
/// can only drain pauses between commands — a pause created inside a
/// dispatch that itself waits on the fetch (Runtime.evaluate of a fetch
/// expression) is undeliverable until that dispatch ends, so without a
/// bound such a call would hang forever. Real clients resolve in
/// milliseconds; the default only shows up on client-less pages. Tests
/// shorten it via the atomic.
pub static INTERCEPT_RESOLUTION_TIMEOUT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(10_000);

pub struct JsState {
    pub dom: Option<DomTree>,
    pub url: String,
    /// WHATWG canonical name of the document's character encoding (e.g.
    /// "UTF-8", "EUC-JP"). Backs `document.characterSet` and the URL query
    /// encoding override for `<a>`/`<area>` hrefs in legacy-charset documents.
    pub encoding: String,
    pub title: String,
    /// URL of the document that initiated this document's navigation. Direct
    /// automation navigations leave this empty; document-initiated navigations
    /// set it per the strict-origin-when-cross-origin policy (upstream
    /// edb1785).
    pub referrer: String,
    /// Referrer Policy delivered by the main response's `Referrer-Policy`
    /// header (last valid comma token). Empty = none delivered; the document
    /// policy then comes from `<meta name=referrer>` (see
    /// resolve_referrer_policy_from), falling back to the spec default.
    pub referrer_policy_header: String,
    pub blocked_urls: Vec<String>,
    pub cookie_jar: Option<Arc<CookieJar>>,
    pub http_client: Option<Arc<HttpClient>>,
    pub pending_navigation: Option<(String, String, String)>,
    pub intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    pub intercept_enabled: bool,
    // Queue of (binding_name, payload) calls made by page JS via the
    // `op_binding_called` op. Drained by the CDP layer after each dispatch
    // and emitted as `Runtime.bindingCalled` events.
    pub pending_binding_calls: Vec<(String, String)>,
    // Queue of (level, message) console calls made by page JS via the
    // `op_console_msg` op. Drained by the CDP layer after each dispatch and
    // emitted as `Runtime.consoleAPICalled` events so console output is
    // visible to Playwright/Puppeteer instead of only the tracing log.
    pub pending_console_calls: Vec<(String, String, String)>,
    /// Set when page JS fed `document.write()` — per HTML spec that parse
    /// produces a fresh document whose `load` fires again (Playwright's
    /// setContent waits on it). Drained by the CDP layer after the console
    /// drain so the setContent tag message clears the frame's lifecycle
    /// state before the new load events land.
    pub pending_write_nav: std::cell::Cell<bool>,
    /// Dialog policy applied by `op_dialog` to subsequent window.confirm/
    /// prompt calls (alert has no answer). Default false = auto-dismiss, so
    /// dialogs can never block: the thread that would show the dialog is the
    /// same one running the page script. Flipped via the session_dialog
    /// command; each dialog is recorded into `pending_console_calls` at
    /// level "dialog" so session_console shows what the page asked.
    pub dialog_accept: bool,
    /// Text returned by window.prompt when a dialog is accepted and this is
    /// set; unset falls back to the call's default argument (or "").
    pub dialog_prompt_text: Option<String>,
    /// The document's input stream for `document.write()`, created on the
    /// first call. Why the calls share one parser is in `write_stream`.
    pub(crate) write_stream: std::cell::RefCell<Option<crate::diting_js::write_stream::DocumentWriteStream>>,
    /// HTML's per-script "already started" flag. This is native page state
    /// rather than wrapper state, because it must survive moves and clones and
    /// because fragment parsing can create nodes before a JS wrapper exists.
    pub(crate) already_started_scripts: RefCell<HashSet<NodeId>>,
    /// Window-global import-map state shared by parser-discovered scripts,
    /// dynamically inserted import maps, and the module loader.
    pub(crate) import_map: Rc<RefCell<crate::diting_js::import_map::ImportMap>>,
    /// Blob bodies mirrored from JS `URL.createObjectURL` (blob URL → bytes
    /// + MIME), shared with the module loader so `import("blob:…")` and
    /// `<script type=module src="blob:…">` resolve URLs no HTTP client can
    /// fetch. Realm-scoped like the JS-side `__blobObjs`.
    pub(crate) blob_store: Rc<RefCell<HashMap<String, (Vec<u8>, String)>>>,
    /// In-flight dynamic `<script src>` fetches. Dynamic scripts fetch via the
    /// op-level reqwest client, invisible to the page-level http_client's
    /// active_requests() counter — without this, the post-script settle loop
    /// exits at its 500ms deadline while a slow external script is still in
    /// flight, and the CDP consumer snapshots before the script (and its
    /// load event) lands (upstream a6bb741).
    pub(crate) dynamic_script_fetches: std::cell::Cell<u32>,
    /// Passive on_request/on_response registry owned by the page this realm
    /// renders. JS fetch()/XHR requests fire it so page-scoped observers see
    /// script-initiated traffic too (upstream #408). None when the runtime
    /// has no owning page (bare module-loader runtimes).
    pub(crate) callbacks: Option<std::sync::Arc<crate::diting_net::CallbackRegistry>>,
    /// Response bodies retained for script-initiated requests (fetch/XHR),
    /// keyed `fetch-{N}` — the same id the paired `js_network_events` entry
    /// carries, so CDP `Network.getResponseBody` resolves. LRU-bounded by
    /// `response_body_entry_limit` / `response_body_byte_limit`.
    pub(crate) network_response_bodies: std::collections::HashMap<String, StoredNetworkResponseBody>,
    pub(crate) network_response_body_order: std::collections::VecDeque<String>,
    pub(crate) network_response_body_counter: u64,
    /// Network events recorded for script-initiated requests (fetch/XHR),
    /// drained into the owning Page's `network_events` by
    /// `Page::sync_js_network_events` so the CDP layer emits
    /// requestWillBeSent / responseReceived for them (upstream #406).
    pub(crate) js_network_events: Vec<JsNetworkEvent>,
    /// Live page-facing WebSocket sockets (registry + channels, op land
    /// owned); see `diting_js::ws`.
    pub(crate) ws_registry: crate::diting_js::ws::WsRegistry,
    /// Memoized diting-layout run for the live DOM tree, keyed by the
    /// tree's epoch (see DomTree::epoch): element rects, the paint order,
    /// and the cascaded ComputedStyle per element. Filled on the first
    /// `layout_rect` / `paint_order` / `computed_style` op after each
    /// mutation; backs getBoundingClientRect, elementFromPoint and
    /// getComputedStyle.
    #[cfg(feature = "screenshot")] // the layout ops run only in the screenshot-gated pipeline
    layout_cache: std::cell::RefCell<Option<(u64, LayoutRun)>>,
    /// Memoized taffy solve for the live DOM tree, keyed by the same epoch
    /// — the geometry half of a layout run (see `SolvedGeometry`). A
    /// paint-only style write (transform/opacity, everything a timeline
    /// seek lands) drops `layout_cache` but keeps this one: the solve
    /// provably ignores those properties, so the next run re-collects
    /// against the cached tree instead of re-solving the full page.
    #[cfg(feature = "screenshot")]
    geometry_cache:
        std::cell::RefCell<Option<(u64, crate::diting_layout::SolvedGeometry)>>,
    /// Memoized layout run for an orphan subtree (a fabricated iframe
    /// document, obscura #976 family), keyed (epoch, subtree root). Kept
    /// apart from `layout_cache` on purpose: a sub-run's rects answer in
    /// the iframe's OWN viewport (the 300x150 default box the fabricated
    /// windows publish), never the main page's.
    #[cfg(feature = "screenshot")]
    iframe_layout_cache:
        std::cell::RefCell<Option<(u64, NodeId, std::rc::Rc<LayoutRun>)>>,
    /// Full taffy solves this state has run — a test probe, so the
    /// paint-only path can assert it stays flat across seeks.
    #[cfg(feature = "screenshot")]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) solves: std::cell::Cell<u64>,
    /// Band paints this state has produced — a test probe, so the video
    /// pump's static-frame reuse can assert held frames skip the paint.
    #[cfg(feature = "screenshot")]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) band_paints: std::cell::Cell<u64>,
    /// External stylesheet bodies fetched at navigation (absolute URL →
    /// decoded CSS text). Joined into the cascade by [`layout_run_all`]
    /// and served to the JS side as `document.styleSheets` rule content.
    pub(crate) ext_sheets: std::cell::RefCell<std::collections::HashMap<String, String>>,
    /// Viewport the layout pipeline should anchor the initial containing
    /// block to, published by the JS persona (`__diting_setPersona`) so
    /// getBoundingClientRect agrees with window.innerWidth/innerHeight.
    /// Defaults to the bootstrap's pre-persona 1920x1000.
    #[cfg(feature = "screenshot")]
    pub(crate) viewport: (f32, f32),
    /// Emulated media environment pushed via CDP `Emulation.setEmulatedMedia`
    /// (Playwright's `page.emulateMedia`): the media type + `prefers-*`
    /// overrides the @media cascade re-parses against — the Rust face of the
    /// JS `matchMedia` truth tables (two faces, one truth).
    #[cfg(feature = "screenshot")]
    pub(crate) media_type: crate::diting_css::CssMediaType,
    #[cfg(feature = "screenshot")]
    pub(crate) media_overrides: crate::diting_css::MediaOverrides,
    /// The root scroller's offset, mirrored from the bootstrap's
    /// scrollTop/scrollLeft setters (viewport roots only) so the CDP frame
    /// pump can paint the viewport band without re-serializing the DOM.
    /// Layout is scroll-blind, so this never feeds the layout cache.
    #[cfg(feature = "screenshot")]
    pub(crate) scroll_offset: (f32, f32),
    /// CSS animation clock in seconds. `None` = static render, which samples
    /// every animation at its end state (animated SVGs show the finished
    /// diagram, not the blank t=0 frame). When the video pump drives it, the
    /// sampler interpolates keyframes at that time. Sampled props are
    /// paint-only (opacity/transform/stroke-dashoffset), so advancing it only
    /// drops the paint caches, never the taffy solve (#395).
    #[cfg(feature = "screenshot")]
    pub(crate) css_time: Option<f64>,
    /// Max (delay + duration) over every element's computed `animation`,
    /// recomputed each layout run. The video pump uses it as the timeline
    /// extent for CSS-animated pages (no `__timelines` needed).
    #[cfg(feature = "screenshot")]
    pub(crate) css_extent: std::cell::Cell<f64>,
    /// Registered property transitions (CSS transitions batch): the JS
    /// face snapshots a tracked-property style write and registers one
    /// entry per (nid, property); a re-trigger on the same pair replaces
    /// the old entry. Sampled at the exit of compute_styles_timed.
    #[cfg(feature = "screenshot")]
    pub(crate) css_transitions:
        std::cell::RefCell<Vec<crate::diting_css::CssTransition>>,
    /// Layout invalidation revision: bumped wherever `layout_cache` is
    /// dropped. The DomTree epoch is a tree-shape stamp — attribute-level
    /// writes (style/class/attr) clear the cache without allocating nodes,
    /// so the epoch alone never sees them. Consumers deciding "did layout
    /// change" from the epoch (the screencast damage signature) must fold
    /// this rev in, or a style change freezes the cast while layout probes
    /// report fresh geometry (the AginxOS five-mutation report).
    #[cfg(feature = "screenshot")]
    pub(crate) layout_rev: std::cell::Cell<u64>,
    /// Attribute names referenced by attribute selectors in the last parsed
    /// rule pool, lowercased (obscura#983). Lets a write to a layout-inert
    /// attribute skip the layout-cache drop; `None` until the first layout
    /// run (no caches exist yet, so skipping is free either way).
    #[cfg(feature = "screenshot")]
    pub(crate) attr_selector_names:
        std::cell::RefCell<Option<std::collections::HashSet<String>>>,
    /// Absolute-URL → fetched image body, filled on demand by the CDP
    /// viewport capture / screencast pump so band paint renders real rasters
    /// instead of placeholders (the outerHTML re-render path pre-fetches;
    /// the live-tree path fills lazily). Bounded entry-wise; every insert
    /// also invalidates `layout_cache` (placeholder boxes vs real intrinsic
    /// sizes can reflow).
    #[cfg(feature = "screenshot")]
    pub(crate) image_bytes:
        std::cell::RefCell<HashMap<String, std::sync::Arc<Vec<u8>>>>,
    /// Insertion order for `image_bytes` (FIFO eviction at the entry cap —
    /// clearing wholesale would thrash pages with more images than the cap).
    #[cfg(feature = "screenshot")]
    pub(crate) image_order: std::cell::RefCell<std::collections::VecDeque<String>>,
    /// Sticky (v2): per-node scroll-dependent shift map ([dx, dy] in
    /// document space), memoized per (epoch, layout_rev, root scroll,
    /// viewport, scroll_gen) — scroll and viewport moves don't bump the
    /// epoch, layout_rev covers attr-level writes the epoch can't see (the
    /// same pairing as the screencast damage signature), and scroll_gen
    /// covers element-scroller writes, which bump neither. Cleared
    /// alongside the layout caches in drop_layout. See `sticky_shifts`.
    #[cfg(feature = "screenshot")]
    pub(crate) sticky_shift_cache: std::cell::RefCell<
        Option<(
            (u64, u64, f32, f32, f32, f32, u64),
            std::rc::Rc<HashMap<NodeId, [f32; 2]>>,
        )>,
    >,
}

/// A script-initiated request as a CDP-shaped network event. Static
/// navigation subresources go through Page::record_network_event; this is
/// the parallel channel for script-initiated requests, which run in the V8
/// op layer and would otherwise never surface as Network events (#406).
#[cfg_attr(not(test), allow(dead_code))] // tests assert every field; /network endpoint is the pending reader
#[derive(Debug, Clone)]
pub struct JsNetworkEvent {
    /// Matches the `fetch-{N}` id under which the body is stored, so CDP
    /// Network.getResponseBody resolves for the same request.
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub response_headers: HashMap<String, String>,
    pub body_size: usize,
    pub timestamp: f64,
    /// Why a `status: 0` entry never produced a servable response — SSRF
    /// block, CORS refusal, transport failure. Chrome DevTools annotates
    /// failed rows with the reason; without it a page whose API calls all
    /// die here reads as "never issued a request" (the taobao shop-SPA
    /// report chased exactly that ghost).
    pub error: Option<String>,
}

/// A response body retained for `Network.getResponseBody`. Bodies are
/// classified with the Chromium DevTools policy (see
/// `diting_net::decode_devtools_body`): replacement-free text is stored as a
/// string (`base64_encoded = false`, declared-GBK pages included, matching
/// Chrome); opaque or undecodable bodies are stored base64.
#[derive(Debug, Clone)]
pub struct StoredNetworkResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

fn response_body_entry_limit() -> usize {
    std::env::var("AGINXBROWSER_NETWORK_BODY_BUFFER_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

fn response_body_byte_limit() -> usize {
    std::env::var("AGINXBROWSER_NETWORK_BODY_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024)
}

/// Hard ceiling on a single JS fetch()/XHR response body (upstream #581).
/// Unlike the two limits above — which bound what is *retained* for CDP —
/// this one bounds the initial allocation itself: page JS can fetch() any
/// URL, so a server streaming gigabytes must fail as a network error
/// instead of OOMing the process (plus the UTF-8 and base64 copies that
/// follow the raw buffer). Bulk transfer is the streaming download layer's
/// job, not page fetch(). `0` disables the cap, matching the 0-disables
/// convention of the two limits above. Shared with the ES module loader
/// (obscura #849) so page-controlled remote code has one size policy.
pub(crate) fn fetch_body_byte_limit() -> usize {
    std::env::var("AGINXBROWSER_FETCH_BODY_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(|v| if v == 0 { usize::MAX } else { v })
        .unwrap_or(64 * 1024 * 1024)
}

impl JsState {
    /// Drop the memoized layout run and stamp the invalidation revision.
    /// Every cache-drop site must go through here — a bare cache drop hides
    /// the invalidation from epoch-keyed consumers like the screencast
    /// pump's damage signature.
    #[cfg(feature = "screenshot")]
    pub(crate) fn drop_layout(&self) {
        *self.layout_cache.borrow_mut() = None;
        *self.geometry_cache.borrow_mut() = None;
        // A style/class write inside a fabricated iframe document doesn't
        // bump the tree epoch, so the (epoch, root) key can't see it — the
        // shared drop points are the only reliable invalidation.
        *self.iframe_layout_cache.borrow_mut() = None;
        // Sticky shifts key on (epoch, layout_rev, scroll, viewport); the
        // rev bump below kills the entry either way — clear for tidiness.
        *self.sticky_shift_cache.borrow_mut() = None;
        self.layout_rev.set(self.layout_rev.get().wrapping_add(1));
    }

    /// The paint-only invalidation (#395): a transform/opacity style write
    /// changes pixels but not geometry — the taffy solve provably ignores
    /// both (`to_taffy_style` maps only geometry families), so the solve
    /// cache survives and only the collected half re-runs. `layout_rev`
    /// still moves: the pixels DID change, and damage-signature consumers
    /// key on it.
    #[cfg(feature = "screenshot")]
    pub(crate) fn drop_paint_only(&self) {
        *self.layout_cache.borrow_mut() = None;
        *self.iframe_layout_cache.borrow_mut() = None;
        self.layout_rev.set(self.layout_rev.get().wrapping_add(1));
    }

    pub fn new() -> Self {
        JsState {
            dom: None,
            url: "about:blank".to_string(),
            encoding: "UTF-8".to_string(),
            title: String::new(),
            referrer: String::new(),
            referrer_policy_header: String::new(),
            blocked_urls: Vec::new(),
            cookie_jar: None,
            http_client: None,
            pending_navigation: None,
            intercept_tx: None,
            intercept_enabled: false,
            pending_binding_calls: Vec::new(),
            pending_console_calls: Vec::new(),
            pending_write_nav: std::cell::Cell::new(false),
            dialog_accept: false,
            dialog_prompt_text: None,
            write_stream: std::cell::RefCell::new(None),
            already_started_scripts: RefCell::new(HashSet::new()),
            import_map: Rc::new(RefCell::new(crate::diting_js::import_map::ImportMap::default())),
            blob_store: Rc::new(RefCell::new(HashMap::new())),
            dynamic_script_fetches: std::cell::Cell::new(0),
            callbacks: None,
            network_response_bodies: std::collections::HashMap::new(),
            network_response_body_order: std::collections::VecDeque::new(),
            network_response_body_counter: 0,
            js_network_events: Vec::new(),
            ws_registry: Default::default(),
            // Memoized diting-layout rects for the live DOM tree, keyed by
            // the tree's epoch (see DomTree::epoch). Filled on the first
            // `layout_rect` op after each mutation; backs getBoundingClientRect.
            #[cfg(feature = "screenshot")]
            layout_cache: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            geometry_cache: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            iframe_layout_cache: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            solves: std::cell::Cell::new(0),
            #[cfg(feature = "screenshot")]
            band_paints: std::cell::Cell::new(0),
            #[cfg(feature = "screenshot")]
            viewport: (1920.0, 1000.0),
            #[cfg(feature = "screenshot")]
            media_type: crate::diting_css::CssMediaType::Screen,
            #[cfg(feature = "screenshot")]
            media_overrides: crate::diting_css::MediaOverrides::default(),
            #[cfg(feature = "screenshot")]
            scroll_offset: (0.0, 0.0),
            #[cfg(feature = "screenshot")]
            sticky_shift_cache: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            css_time: None,
            #[cfg(feature = "screenshot")]
            css_extent: std::cell::Cell::new(0.0),
            #[cfg(feature = "screenshot")]
            css_transitions: std::cell::RefCell::new(Vec::new()),
            #[cfg(feature = "screenshot")]
            layout_rev: std::cell::Cell::new(0),
            #[cfg(feature = "screenshot")]
            attr_selector_names: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            image_bytes: std::cell::RefCell::new(HashMap::new()),
            #[cfg(feature = "screenshot")]
            image_order: std::cell::RefCell::new(std::collections::VecDeque::new()),
            // External stylesheet bodies fetched at navigation, keyed by
            // absolute URL. The layout run joins them with the live <style>
            // blocks in document order — without this table an author sheet
            // that arrives over <link> never reaches the cascade, so
            // getComputedStyle answers initial values and every rect-based
            // consumer (gBCR, elementFromPoint) sees unstyled geometry.
            ext_sheets: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }
}

pub type SharedState = Rc<RefCell<JsState>>;

pub(crate) fn node_is_script(dom: &DomTree, node_id: NodeId) -> bool {
    dom.with_node(node_id, |node| {
        node.as_element()
            .map(|name| name.local.as_ref().eq_ignore_ascii_case("script"))
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

fn script_nodes_including_template_contents(dom: &DomTree, root: NodeId) -> Vec<NodeId> {
    let mut scripts = Vec::new();
    let mut stack = vec![root];
    while let Some(node_id) = stack.pop() {
        if node_is_script(dom, node_id) {
            scripts.push(node_id);
        }
        let template_contents = dom
            .with_node(node_id, |node| match &node.data {
                NodeData::Element { template_contents, .. } => *template_contents,
                _ => None,
            })
            .flatten();
        if let Some(contents) = template_contents {
            stack.push(contents);
        }
        let children = dom.children(node_id);
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    scripts
}

pub(crate) fn mark_script_subtree_started(state: &JsState, root: NodeId) {
    let Some(dom) = state.dom.as_ref() else {
        return;
    };
    let scripts = script_nodes_including_template_contents(dom, root);
    state.already_started_scripts.borrow_mut().extend(scripts);
}

fn propagate_script_start_state(
    dom: &DomTree,
    source_root: NodeId,
    cloned_root: NodeId,
    started: &RefCell<HashSet<NodeId>>,
) {
    let mut pairs = vec![(source_root, cloned_root)];
    let mut additions = Vec::new();
    let current = started.borrow();
    while let Some((source, cloned)) = pairs.pop() {
        if current.contains(&source) {
            additions.push(cloned);
        }

        let source_template = dom
            .with_node(source, |node| match &node.data {
                NodeData::Element { template_contents, .. } => *template_contents,
                _ => None,
            })
            .flatten();
        let cloned_template = dom
            .with_node(cloned, |node| match &node.data {
                NodeData::Element { template_contents, .. } => *template_contents,
                _ => None,
            })
            .flatten();
        if let (Some(source_contents), Some(cloned_contents)) =
            (source_template, cloned_template)
        {
            pairs.push((source_contents, cloned_contents));
        }

        let source_children = dom.children(source);
        let cloned_children = dom.children(cloned);
        for pair in source_children.into_iter().zip(cloned_children).rev() {
            pairs.push(pair);
        }
    }
    drop(current);
    started.borrow_mut().extend(additions);
}

#[op2(fast)]
fn op_script_mark_started(state: &OpState, nid: u32) -> bool {
    let shared = state.borrow::<SharedState>().clone();
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return false;
    };
    let node_id = NodeId::new(nid);
    if !node_is_script(dom, node_id) {
        return false;
    }
    state.already_started_scripts.borrow_mut().insert(node_id);
    true
}

/// Atomically claim an executable script. A false result means the node was
/// created inert by an HTML-string API or has already been prepared once.
#[op2(fast)]
fn op_script_try_start(state: &OpState, nid: u32) -> bool {
    let shared = state.borrow::<SharedState>().clone();
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return false;
    };
    let node_id = NodeId::new(nid);
    if !node_is_script(dom, node_id) {
        return false;
    }
    let newly_started = state.already_started_scripts.borrow_mut().insert(node_id);
    newly_started
}

/// Bracket a dynamic `<script src>` fetch so the settle loop can distinguish
/// "a script is still loading" from ordinary background XHR/fetch activity.
/// The page-level http_client's active_requests() counter never sees these —
/// they ride the op-level client cache — so without this bracket the settle
/// loop's fast path would strand scripts slower than its 500ms budget.
#[op2(fast)]
fn op_dyn_script_fetch_begin(state: &OpState) {
    let shared = state.borrow::<SharedState>().clone();
    let state = shared.borrow();
    state.dynamic_script_fetches.set(state.dynamic_script_fetches.get() + 1);
}

#[op2(fast)]
fn op_dyn_script_fetch_end(state: &OpState) {
    let shared = state.borrow::<SharedState>().clone();
    let state = shared.borrow();
    state.dynamic_script_fetches.set(state.dynamic_script_fetches.get().saturating_sub(1));
}

/// Mirror a `URL.createObjectURL` registration into the Rust-side blob
/// table. The JS-side `__blobObjs` map is unreachable from the module
/// loader (which resolves outside the realm), so `import("blob:…")` and
/// `<script type=module src="blob:…">` resolve through this mirror.
#[op2(fast)]
fn op_blob_register(
    state: &OpState,
    #[string] id: String,
    #[buffer] bytes: &[u8],
    #[string] mime: String,
) {
    let shared = state.borrow::<SharedState>().clone();
    shared.borrow().blob_store.borrow_mut().insert(id, (bytes.to_vec(), mime));
}

/// Drop a mirrored blob registration (`URL.revokeObjectURL`).
#[op2(fast)]
fn op_blob_revoke(state: &OpState, #[string] id: String) {
    let shared = state.borrow::<SharedState>().clone();
    shared.borrow().blob_store.borrow_mut().remove(&id);
}

/// Attach a native shadow root to a host element (`Element.prototype.attachShadow`).
/// Returns the root's node id, or -2 when the host already has a shadow root
/// (JS maps that to the spec's NotSupportedError) and -1 for any other failure
/// (missing DOM, non-element host, bad mode).
#[op2(fast)]
fn op_shadow_attach(state: &OpState, host_nid: u32, #[string] mode: String) -> i32 {
    let gs = state.borrow::<SharedState>().clone();
    let gs = gs.borrow();
    let dom = match &gs.dom {
        Some(d) => d,
        None => return -1,
    };
    let mode = match mode.as_str() {
        "open" => ShadowRootMode::Open,
        "closed" => ShadowRootMode::Closed,
        _ => return -1,
    };
    match dom.attach_shadow_root(NodeId::new(host_nid), mode) {
        Ok(root) => root.raw() as i32,
        Err(AttachShadowError::HostAlreadyHasShadowRoot) => -2,
        Err(_) => -1,
    }
}

#[op2]
#[string]
fn op_dom(state: &OpState, #[string] cmd: String, #[string] arg1: String, #[string] arg2: String) -> String {
    // Anti-panic boundary: a panic in a DOM op would unwind through deno_core
    // into V8's FFI frame, where V8_Fatal calls abort(3) and takes the whole
    // engine (and every CDP client) down. Catch it so one malformed selector or
    // inconsistent tree node degrades to a null result for that single call.
    // No per-call clone: on the happy path this is just a landing pad, so the
    // hot DOM path (querySelector/getAttribute/...) pays nothing measurable.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        op_dom_inner(state, cmd, arg1, arg2)
    }))
    .unwrap_or_else(|_| {
        tracing::error!("op_dom panicked; returning null");
        "null".to_string()
    })
}

fn op_dom_inner(state: &OpState, cmd: String, arg1: String, arg2: String) -> String {
    // Title write goes to shared state (the getter reads gs.title), so it must
    // happen before the immutable borrow below — and before the `gs.dom` early
    // return, since a document always has a title even with an empty tree.
    if cmd == "set_document_title" {
        let gs = state.borrow::<SharedState>().clone();
        gs.borrow_mut().title = arg1;
        return "null".into();
    }
    // Style/tree writes don't all bump the DomTree epoch (attribute-level
    // mutations don't allocate nodes), so the memoized layout run must be
    // dropped explicitly at the write commands — the bootstrap mirrors
    // this set as _DOM_MUTATION_COMMANDS for its getComputedStyle
    // snapshot epoch, and both sides see the same op_dom traffic through
    // the same choke point.
    #[cfg(feature = "screenshot")]
    if matches!(
        cmd.as_str(),
        "append_child"
            | "insert_before"
            | "remove_child"
            | "set_attribute"
            | "remove_attribute"
            | "set_text_content"
            | "set_inner_html"
            | "set_live_value"
            | "set_live_checked"
            | "set_focused"
            | "set_selection"
            | "document_write_reset"
    ) {
        let gs = state.borrow::<SharedState>().clone();
        // Paint-only style write (#395): a timeline seek lands as
        // set_attribute("style", …) whose property-NAME diff sits entirely
        // inside {transform, opacity} — exactly the compositor-only set a
        // real browser never re-layouts for. The taffy solve ignores both,
        // so it survives and the next layout run re-collects against the
        // cached tree (measured 109ms/frame full-pipeline seeks on the
        // probe page → the paint floor). This runs pre-write, so the OLD
        // attribute is still on the node for the diff. Anything else —
        // unknown properties added, no prior style to diff against — falls
        // back to the full drop.
        //
        // Value-identity short-circuit (#398): a style write whose full
        // serialized string is byte-identical to what's already on the node
        // cannot change any computed style, so no cache drops and layout_rev
        // holds — that flat revision is what the video pump's static-frame
        // reuse keys on. The DOM write below still happens (observers see
        // it, exactly as Chrome fires mutation records for a redundant
        // setAttribute); only the invalidation is skipped, the same way
        // Chrome's style system dedupes equal declarations.
        let mut paint_only = false;
        let mut identity = false;
        if cmd.as_str() == "set_attribute"
            && arg2
                .split_once('\0')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("style"))
        {
            let old = arg1
                .parse::<u32>()
                .ok()
                .map(NodeId::new)
                .and_then(|id| {
                    gs.borrow()
                        .dom
                        .as_ref()
                        .and_then(|d| d.get_node(id).and_then(|n| n.get_attribute("style").map(|s| s.to_string())))
                });
            let new = arg2.split_once('\0').map(|(_, v)| v);
            identity = old.as_deref() == new;
            paint_only = !identity && style_write_is_paint_only(old.as_deref(), new);
        }
        // Inert attribute writes (obscura#983): tabindex/title/role/
        // data-*/aria-* change no computed style and no layout input on
        // their own — a docs theme writing tabindex per heading used to pay
        // a full re-layout per write. Escape hatch: if any loaded rule's
        // selector references the name (`[data-x]`, `[aria-expanded]`), the
        // write still invalidates. Everything layout, pseudo-classes, or
        // fetch triggers read (class/id/style, width/height/colspan/…,
        // disabled/checked/href/src/rel) is deliberately NOT in the inert
        // set. The DOM write below still happens either way.
        let attr_name = match cmd.as_str() {
            "set_attribute" => arg2.split_once('\0').map(|(n, _)| n),
            "remove_attribute" => Some(arg2.as_str()),
            _ => None,
        };
        let inert = attr_name.is_some_and(|n| {
            let gs = gs.borrow();
            let skip = attr_write_is_layout_inert(n, gs.attr_selector_names.borrow().as_ref());
            skip
        });
        if !identity && !inert {
            if paint_only {
                gs.borrow().drop_paint_only();
            } else {
                gs.borrow().drop_layout();
            }
        }
    }
    // Persona viewport: needs a mutable borrow, so it runs before the main
    // read-only `gs` alias below (same pattern as set_document_title).
    #[cfg(feature = "screenshot")]
    if cmd == "set_viewport" {
        let gs = state.borrow::<SharedState>().clone();
        let w = arg1.parse::<f32>().unwrap_or(1920.0);
        let h = arg2.parse::<f32>().unwrap_or(1000.0);
        if w.is_finite() && w > 0.0 && h.is_finite() && h > 0.0 {
            let mut gs = gs.borrow_mut();
            gs.viewport = (w, h);
            // Any rects memoized under the old ICB are stale now.
            gs.drop_layout();
        }
        return "ok".into();
    }
    // Emulated media environment from CDP `Emulation.setEmulatedMedia`
    // (Playwright's page.emulateMedia). The bootstrap recomputes its
    // matchMedia tables first and then pushes the same pairs here, so the
    // cascade face and the script face flip together; @media arms re-parse
    // on the next layout run.
    #[cfg(feature = "screenshot")]
    if cmd == "set_media_env" {
        let gs = state.borrow::<SharedState>().clone();
        let features: Vec<(String, String)> = serde_json::from_str(&arg1).unwrap_or_default();
        let media_type = if arg2.eq_ignore_ascii_case("print") {
            crate::diting_css::CssMediaType::Print
        } else {
            crate::diting_css::CssMediaType::Screen
        };
        {
            let mut gs = gs.borrow_mut();
            gs.media_type = media_type;
            gs.media_overrides.features = features;
            gs.drop_layout();
        }
        return "ok".into();
    }
    // Root-scroller offset mirror from the bootstrap (viewport roots only;
    // element-level scrolling is not mirrored). The bootstrap already clamps
    // negatives to 0; band paint clamps to the scrollable range itself.
    #[cfg(feature = "screenshot")]
    if cmd == "set_scroll_offset" {
        let gs = state.borrow::<SharedState>().clone();
        let x = arg1.parse::<f32>().unwrap_or(0.0);
        let y = arg2.parse::<f32>().unwrap_or(0.0);
        if x.is_finite() && y.is_finite() {
            gs.borrow_mut().scroll_offset = (x.max(0.0), y.max(0.0));
        }
        return "ok".into();
    }
    // CSS animation clock, driven by the video pump. Negative/NaN rejected;
    // t=0 (the pre-delay state) is a legal sample.
    #[cfg(feature = "screenshot")]
    if cmd == "set_css_time" {
        let gs = state.borrow::<SharedState>().clone();
        let t = arg1.parse::<f64>().unwrap_or(f64::NAN);
        if t.is_finite() && t >= 0.0 {
            let mut gs = gs.borrow_mut();
            if gs.css_time != Some(t) {
                gs.css_time = Some(t);
                gs.drop_paint_only();
            }
        }
        return "ok".into();
    }
    // Transition registry (JS face detects the trigger and hands us the
    // before/after computed values). The a2 slot carries a JSON blob —
    // _domRaw only forwards two args. start=0 on the video timeline: page
    // scripts run once during settle, so a registered transition plays from
    // the pump's t=0. Live screenshots keep css_time=None and the sampler
    // no-ops (end state stands) — the settled-posture collapse.
    #[cfg(feature = "screenshot")]
    if cmd == "add_css_transition" {
        let gs = state.borrow::<SharedState>().clone();
        let Some(nid) = arg1.parse::<u32>().ok().map(NodeId::new) else {
            return "ok".into();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&arg2) else {
            return "ok".into();
        };
        let Some(prop) = v
            .get("prop")
            .and_then(|x| x.as_str())
            .map(|s| s.to_ascii_lowercase())
        else {
            return "ok".into();
        };
        let parse_val = |name: &str, s: &str| -> Option<crate::diting_css::TransitionValue> {
            match name {
                "opacity" => s
                    .trim()
                    .parse::<f32>()
                    .ok()
                    .map(|n| crate::diting_css::TransitionValue::Opacity(n.clamp(0.0, 1.0))),
                "color" | "background-color" => crate::diting_css::parse_color(s).map(|c| {
                    crate::diting_css::TransitionValue::Color([
                        c.0 as f32, c.1 as f32, c.2 as f32, c.3 as f32,
                    ])
                }),
                // The computed snapshot is "none" or a `matrix(...)` string;
                // `none` maps to the identity-side arm of lerp_transform so
                // none↔matrix transitions still interpolate. A percentage
                // translate has no used value at snapshot time (the computed
                // table omits it) and fails to parse — no transition.
                "transform" => {
                    let s = s.trim();
                    if s == "none" || s.is_empty() {
                        Some(crate::diting_css::TransitionValue::Transform(None))
                    } else {
                        crate::diting_css::parse_transform(s)
                            .map(|t| crate::diting_css::TransitionValue::Transform(Some(t)))
                    }
                }
                _ => None,
            }
        };
        let (Some(from), Some(to)) = (
            v.get("from")
                .and_then(|x| x.as_str())
                .and_then(|s| parse_val(&prop, s)),
            v.get("to")
                .and_then(|x| x.as_str())
                .and_then(|s| parse_val(&prop, s)),
        ) else {
            return "ok".into();
        };
        let duration = (v.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32) / 1000.0;
        let delay = (v.get("delay").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32) / 1000.0;
        if !duration.is_finite()
            || duration < 0.0
            || !delay.is_finite()
            || delay < 0.0
            || duration + delay <= 0.0
        {
            return "ok".into();
        }
        let easing = v
            .get("easing")
            .and_then(|x| x.as_str())
            .and_then(crate::diting_css::parse_easing_token)
            .unwrap_or(crate::diting_css::Easing::CubicBezier(0.25, 0.1, 0.25, 1.0));
        let gs = gs.borrow_mut();
        let mut list = gs.css_transitions.borrow_mut();
        // A re-trigger on the same (node, property) replaces the pending
        // entry; the cap bounds a page that spams writes every frame.
        list.retain(|tr| !(tr.nid == nid.index() && tr.property == prop));
        if list.len() >= 256 {
            list.remove(0);
        }
        list.push(crate::diting_css::CssTransition {
            nid: nid.index(),
            property: prop,
            from,
            to,
            start: 0.0,
            duration,
            delay,
            easing,
        });
        drop(list);
        gs.drop_paint_only();
        return "ok".into();
    }
    let gs = state.borrow::<SharedState>().clone();
    let gs = gs.borrow();
    let dom = match &gs.dom {
        Some(d) => d,
        None => return "null".to_string(),
    };

    // Node-id args that fail to parse must NOT silently become node 0 (the
    // document root) — a fake-receiver call like
    // `Element.prototype.setHTMLUnsafe.call({})` would otherwise wipe the
    // whole document. Mutating commands no-op on an invalid id.
    let parse_nid = |s: &str| -> Option<NodeId> { s.parse::<u32>().ok().map(NodeId::new) };

    match cmd.as_str() {
        "document_node_id" => dom.document().index().to_string(),
        "document_title" => serde_json::to_string(&gs.title).unwrap_or("\"\"".into()),
        "document_referrer" => serde_json::to_string(&gs.referrer).unwrap_or("\"\"".into()),
        "document_url" => serde_json::to_string(&gs.url).unwrap_or("\"\"".into()),
        // Document referrer policy (Referrer Policy §"Determine request's
        // Referrer Policy"): header-delivered policy beats every <meta
        // name=referrer>; "" = no policy (callers apply the spec default).
        "document_referrer_policy" => {
            let policy = resolve_referrer_policy_from(&gs, dom);
            serde_json::to_string(&policy).unwrap_or("\"\"".into())
        }
        // Document BASE url (HTML §document-base-url): the document URL with
        // the first <base href> folded in. This is what relative URL
        // resolution (anchor/area href, form action, iframe src, fetch) must
        // resolve against — upstream obscura #658. document.URL and origin
        // checks stay on the plain "document_url".
        "document_base_url" => {
            let base = dom
                .query_selector("base[href]")
                .ok()
                .flatten()
                .and_then(|nid| {
                    dom.get_node(nid)
                        .and_then(|n| n.get_attribute("href").map(|v| v.to_string()))
                });
            let folded = base.and_then(|href| {
                url::Url::parse(&gs.url)
                    .ok()
                    .and_then(|doc| doc.join(&href).ok())
                    .map(|u| u.to_string())
            });
            serde_json::to_string(&folded.unwrap_or_else(|| gs.url.clone()))
                .unwrap_or("\"\"".into())
        }
        "document_encoding" => serde_json::to_string(&gs.encoding).unwrap_or("\"UTF-8\"".into()),
        "document_element" => {
            for cid in dom.children(dom.document()) {
                if let Some(n) = dom.get_node(cid) {
                    if n.as_element().map(|name| name.local.as_ref() == "html").unwrap_or(false) {
                        return cid.index().to_string();
                    }
                }
            }
            "-1".into()
        }
        "document_doctype" => {
            for cid in dom.children(dom.document()) {
                if let Some(n) = dom.get_node(cid) {
                    if let crate::diting_dom::NodeData::Doctype { name, public_id, system_id } = &n.data {
                        return serde_json::json!({
                            "name": name,
                            "publicId": public_id,
                            "systemId": system_id,
                            "nodeId": cid.index(),
                        }).to_string();
                    }
                }
            }
            "null".into()
        }
        "get_element_by_id" => {
            dom.get_element_by_id(&arg1).map(|id| id.index().to_string()).unwrap_or("-1".into())
        }
        "query_selector" => {
            dom.query_selector(&arg1).ok().flatten().map(|id| id.index().to_string()).unwrap_or("-1".into())
        }
        "query_selector_all" => {
            let ids: Vec<i32> = dom.query_selector_all(&arg1).ok()
                .map(|ids| ids.iter().map(|id| id.index() as i32).collect()).unwrap_or_default();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "query_selector_scoped" => {
            let root_nid = arg1.parse::<u32>().unwrap_or(0);
            dom.query_selector_from(NodeId::new(root_nid), &arg2).ok().flatten()
                .map(|id| id.index().to_string()).unwrap_or("-1".into())
        }
        "query_selector_all_scoped" => {
            let root_nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom.query_selector_all_from(NodeId::new(root_nid), &arg2).ok()
                .map(|ids| ids.iter().map(|id| id.index() as i32).collect()).unwrap_or_default();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        // Single-element match with `:scope` bound to the element itself
        // (Element.matches semantics). "0" on a parse error, mirroring how the
        // query ops return empty results rather than surfacing the error.
        "matches_selector" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            if dom.matches_selector(NodeId::new(nid), &arg2).unwrap_or(false) { "1".into() } else { "0".into() }
        }
        "node_type" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.get_node(NodeId::new(nid)).map(|n| match &n.data {
                NodeData::Document => "9", NodeData::Element { .. } => "1", NodeData::Text { .. } => "3",
                NodeData::Comment { .. } => "8", NodeData::Doctype { .. } => "10", NodeData::ProcessingInstruction { .. } => "7",
            }).unwrap_or("0").into()
        }
        "node_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name: String = dom.get_node(NodeId::new(nid)).map(|n| match &n.data {
                NodeData::Document => "#document".to_string(), NodeData::Element { name, .. } => if name.ns == html5ever::ns!(html) { name.local.as_ref().to_ascii_uppercase() } else { name.local.as_ref().to_string() },
                NodeData::Text { .. } => "#text".to_string(), NodeData::Comment { .. } => "#comment".to_string(),
                NodeData::Doctype { name, .. } => name.clone(), NodeData::ProcessingInstruction { target, .. } => target.clone(),
            }).unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
        }
        "text_content" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.text_content(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "parent_node" | "first_child" | "last_child" | "next_sibling" | "prev_sibling" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.get_node(NodeId::new(nid)).and_then(|n| match cmd.as_str() {
                "parent_node" => n.parent, "first_child" => n.first_child,
                "last_child" => n.last_child, "next_sibling" => n.next_sibling,
                "prev_sibling" => n.prev_sibling, _ => None,
            }).map(|id| id.index().to_string()).unwrap_or("-1".into())
        }
        "next_in_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.next_in_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "next_after_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.next_after_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "prev_in_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.prev_in_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        // Root of the local tree scope: ordinary parents only, so a shadow
        // descendant resolves to its ShadowRoot (getRootNode default).
        "tree_scope_root" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.tree_scope_root(NodeId::new(nid))
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        // Topmost root after crossing ShadowRoot-to-host edges
        // (getRootNode({ composed: true })).
        "shadow_including_root" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.shadow_including_root(NodeId::new(nid))
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "assigned_slot" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.assigned_slot(NodeId::new(nid))
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "assigned_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            // "null" vs "[]": a slot outside any shadow tree has no
            // assignment AND no fallback; a slot inside one that nothing is
            // assigned to serves its fallback children (the JS side reads
            // the distinction).
            match dom.assigned_nodes(NodeId::new(nid)) {
                Some(ids) => serde_json::to_string(
                    &ids.iter().map(|id| id.index()).collect::<Vec<_>>(),
                )
                .unwrap_or("[]".into()),
                None => "null".into(),
            }
        }
        "child_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom.children(NodeId::new(nid)).iter().map(|id| id.index() as i32).collect();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "tag_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name = dom.get_node(NodeId::new(nid)).and_then(|n| n.as_element().map(|name|
                // HTML elements read uppercase (Chrome tagName convention);
                // XML-namespace elements keep their source case.
                if name.ns == html5ever::ns!(html) { name.local.as_ref().to_ascii_uppercase() } else { name.local.as_ref().to_string() }
            )).unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
        }
        // True per-node namespace from the tree (parsed SVG children inherit
        // theirs from the html5ever foreign-content parse; create_element
        // records what the caller passed). The JS-side namespaceURI getter
        // reads this — a JS-only heuristic used to report XHTML for every
        // parsed <path>/<g>/... (#28).
        "namespace_uri" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ns = dom
                .get_node(NodeId::new(nid))
                .and_then(|n| n.as_element().map(|name| name.ns.as_ref().to_string()));
            serde_json::to_string(&ns).unwrap_or("null".into())
        }
        "get_attribute" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom.get_node(NodeId::new(nid)).and_then(|n| n.get_attribute(&arg2).map(|s| s.to_string()));
            serde_json::to_string(&val).unwrap_or("null".into())
        }
        "attribute_names" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let names: Vec<String> = dom
                .get_node(NodeId::new(nid))
                .map(|n| {
                    n.attrs()
                        .map(|a| a.iter().map(|x| x.name.local.as_ref().to_string()).collect())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            serde_json::to_string(&names).unwrap_or("[]".into())
        }
        "set_attribute" => {
            let node_id = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            if let Some((name, value)) = arg2.split_once('\0') {
                if name == "id" {
                    let old_id = dom.get_node(node_id).and_then(|n| n.get_attribute("id").map(|s| s.to_string()));
                    dom.with_node_mut(node_id, |n| n.set_attribute(name, value.to_string()));
                    dom.update_id_index(node_id, old_id.as_deref(), Some(value));
                } else {
                    dom.with_node_mut(node_id, |n| n.set_attribute(name, value.to_string()));
                }
            }
            "true".into()
        }
        // The form-control dirty value mirrored from the bootstrap's value
        // setter (see NodeData::Element::live_value). Not an attribute on
        // purpose: getAttribute/outerHTML must keep showing the ORIGINAL
        // value attribute like Chrome, while paint reads this instead.
        "set_live_value" => {
            let node_id = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            dom.with_node_mut(node_id, |n| n.set_live_value(arg2.to_string()));
            "true".into()
        }
        // The checkable-input dirty checkedness mirror, same posture as
        // set_live_value: paint reads it to draw the checkbox/radio widget
        // state, while getAttribute/outerHTML keep showing only the parsed
        // `checked` attribute like Chrome.
        "set_live_checked" => {
            let node_id = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            dom.with_node_mut(node_id, |n| n.set_live_checked(arg2 == "1"));
            "true".into()
        }
        // Focus mirror (blitz#839): the bootstrap's focus()/blur() keep the
        // JS-side __diting_focused global for activeElement, and this op
        // mirrors the same fact into the tree so :focus/:focus-within/
        // :focus-visible selectors match on the next style run. arg2 "0"
        // clears (the JS side passes the node it is blurring; a blur when
        // some OTHER node holds focus must not steal it away).
        "set_focused" => {
            let node_id = match parse_nid(&arg1) {
                Some(id) => id,
                None => return "false".into(),
            };
            if arg2 == "1" {
                dom.set_focused_node(Some(node_id));
            } else if dom.focused_node() == Some(node_id) {
                dom.set_focused_node(None);
            }
            "true".into()
        }
        // Selection mirror (caret batch): the bootstrap's selection APIs
        // keep (start, end) in WeakMaps for JS reads; this op mirrors the
        // same record into the tree so paint can resolve the caret at
        // collect time. arg2 is "start,end" — the JS numbers verbatim;
        // paint clamps to the value's char count (UTF-16 vs chars diverge
        // on astral text, accepted).
        "set_selection" => {
            let node_id = match parse_nid(&arg1) {
                Some(id) => id,
                None => return "false".into(),
            };
            let Some((start, end)) = arg2.split_once(',') else {
                return "false".into();
            };
            let (Ok(start), Ok(end)) = (start.trim().parse::<usize>(), end.trim().parse::<usize>())
            else {
                return "false".into();
            };
            dom.set_selection(Some((node_id, start, end)));
            "true".into()
        }
        "inner_html" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.inner_html(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "outer_html" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.outer_html(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "append_child" => {
            let (parent, child) = match (parse_nid(&arg1), parse_nid(&arg2)) {
                (Some(p), Some(c)) => (p, c),
                _ => return "false".into(),
            };
            dom.append_child(parent, child);
            "true".into()
        }
        "remove_child" => {
            let child = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            dom.remove_child(child);
            "true".into()
        }
        "insert_before" => {
            let (new_node, ref_node) = match (parse_nid(&arg1), parse_nid(&arg2)) {
                (Some(n), Some(r)) => (n, r),
                _ => return "false".into(),
            };
            dom.insert_before(ref_node, new_node);
            "true".into()
        }
        "remove_attribute" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            dom.with_node_mut(nid, |n| {
                if let NodeData::Element { attrs, .. } = &mut n.data {
                    attrs.retain(|a| a.name.local.as_ref() != arg2.as_str());
                }
            });
            "true".into()
        }
        "set_inner_html" => {
            let target = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            let children = dom.children(target);
            for child in children {
                dom.detach(child);
            }
            if !arg2.is_empty() {
                let context_name = dom
                    .with_node(target, |node| match &node.data {
                        NodeData::Element { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .flatten();
                let fragment = match context_name {
                    Some(name) => crate::diting_dom::parse_fragment_with_context(&arg2, name),
                    None => crate::diting_dom::parse_fragment(&arg2),
                };
                let import_root = fragment.fragment_root();
                dom.import_children_from(target, &fragment, import_root);
                // innerHTML-created scripts are inert per spec — mark them
                // started so a later move/clone never executes them.
                for child in dom.children(target) {
                    mark_script_subtree_started(&gs, child);
                }
            }
            "true".into()
        }
        // document.write() feeds the document's input stream, so the calls
        // share one parser and one tokenizer state. Returns the nodes that
        // became complete with this call as [[parent, node], …], parents
        // before children; a `parent` of 0 means the node belongs at the
        // insertion point, which the JS caller knows. Nothing is inserted
        // here: insertion must go through Node.appendChild on the JS side,
        // which also reports the mutation and runs written scripts.
        "document_write" => {
            gs.pending_write_nav.set(true);
            let mut slot = gs.write_stream.borrow_mut();
            let stream = slot.get_or_insert_with(crate::diting_js::write_stream::DocumentWriteStream::new);
            let pairs: Vec<[i32; 2]> = stream
                .write(&arg2, dom)
                .iter()
                .map(|placement| {
                    [
                        placement.parent.map_or(0, |id| id.index() as i32),
                        placement.node.index() as i32,
                    ]
                })
                .collect();
            serde_json::to_string(&pairs).unwrap_or("[]".into())
        }
        // document.open() discards what the input stream holds and starts over.
        "document_write_reset" => {
            *gs.write_stream.borrow_mut() = None;
            "true".into()
        }
        "set_text_content" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "false".into() };
            dom.with_node_mut(nid, |n| {
                match &mut n.data {
                    NodeData::Text { contents } => { *contents = arg2.clone(); }
                    NodeData::Comment { contents } => { *contents = arg2.clone(); }
                    NodeData::ProcessingInstruction { data, .. } => { *data = arg2.clone(); }
                    _ => {}
                }
            });
            "true".into()
        }
        "create_document_fragment" => {
            dom.new_node(NodeData::Document).index().to_string()
        }
        "clone_node" => {
            let nid = match arg1.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "-1".into(),
            };
            let source = NodeId::new(nid);
            match dom.clone_node(source, arg2 == "true") {
                Some(cloned) => {
                    propagate_script_start_state(
                        dom,
                        source,
                        cloned,
                        &gs.already_started_scripts,
                    );
                    cloned.index().to_string()
                }
                None => "-1".into(),
            }
        }
        "template_contents" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.template_contents(NodeId::new(nid))
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "create_element" => {
            // arg2 records a namespace on the node. The HTML path passes the
            // XHTML namespace explicitly (keeping tag_name/node_name's
            // uppercase convention); DOMParser's XML tree builder passes the
            // xmlns-resolved namespace, and an empty string is the null
            // namespace — what XML semantics require for elements with no
            // xmlns in scope. Serialization keys off this distinction: HTML
            // void elements self-close, XML elements always keep their
            // closing tag and text children.
            let ns = if arg2 == "http://www.w3.org/1999/xhtml" {
                html5ever::ns!(html)
            } else {
                html5ever::Namespace::from(arg2.as_str())
            };
            dom.new_node(NodeData::Element {
                name: html5ever::QualName::new(None, ns, html5ever::LocalName::from(arg1.as_str())),
                attrs: vec![], template_contents: None, mathml_annotation_xml_integration_point: false,
                live_value: None,
                live_checked: None,
            }).index().to_string()
        }
        "create_text_node" => {
            dom.new_node(NodeData::Text { contents: arg1.clone() }).index().to_string()
        }
        "create_comment_node" => {
            dom.new_node(NodeData::Comment { contents: arg1.clone() }).index().to_string()
        }
        "create_processing_instruction" => {
            // arg1 = target, arg2 = data
            dom.new_node(NodeData::ProcessingInstruction {
                target: arg1.clone(),
                data: arg2.clone(),
            }).index().to_string()
        }
        "create_doctype" => {
            // arg1 = name, arg2 = public_id. system_id stored only in the
            // JS wrapper since neither current WPT test reads it back from
            // the underlying tree.
            dom.new_node(NodeData::Doctype {
                name: arg1.clone(),
                public_id: arg2.clone(),
                system_id: String::new(),
            }).index().to_string()
        }
        "pi_target" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom.get_node(NodeId::new(nid)).and_then(|n| match &n.data {
                NodeData::ProcessingInstruction { target, .. } => Some(target.clone()),
                _ => None,
            }).unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "doctype_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom.get_node(NodeId::new(nid)).and_then(|n| match &n.data {
                NodeData::Doctype { name, .. } => Some(name.clone()),
                _ => None,
            }).unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "doctype_public_id" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom.get_node(NodeId::new(nid)).and_then(|n| match &n.data {
                NodeData::Doctype { public_id, .. } => Some(public_id.clone()),
                _ => None,
            }).unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "element_children" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom.children(NodeId::new(nid)).iter()
                .filter(|&&id| dom.get_node(id).map(|n| n.is_element()).unwrap_or(false))
                .map(|id| id.index() as i32).collect();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "has_child_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.get_node(NodeId::new(nid)).map(|n| n.first_child.is_some()).unwrap_or(false).to_string()
        }
        "contains" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let other = arg2.parse::<u32>().unwrap_or(0);
            dom.descendants(NodeId::new(nid)).contains(&NodeId::new(other)).to_string()
        }
        // Index of a node among its parent's children. Walks prev siblings in
        // Rust, avoiding the per-step JS->op round trips a Range comparison
        // would otherwise make.
        "node_index" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            node_child_index(dom, NodeId::new(nid)).to_string()
        }
        // Document (preorder) tree order of two nodes: -1 if a precedes b, 1 if
        // a follows b, 0 if equal. Used by the Range boundary-point algorithms.
        "compare_order" => {
            let a = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let b = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            compare_node_order(dom, a, b).to_string()
        }
        // Root (topmost ancestor) of a node, in one op rather than an O(depth)
        // walk of parentNode ops from JS.
        "node_root" => {
            let mut cur = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            while let Some(p) = dom.get_node(cur).and_then(|x| x.parent) {
                cur = p;
            }
            cur.index().to_string()
        }
        // External stylesheet bodies (absolute URL → CSS text) fetched at
        // navigation. document.styleSheets reads this to build real
        // CSSStyleSheet entries for <link rel=stylesheet> elements — the
        // fetch itself lives in Page's navigation pipeline, this is the
        // retention side.
        "ext_sheets" => {
            let map = gs.ext_sheets.borrow();
            let mut obj = serde_json::Map::with_capacity(map.len());
            for (k, v) in map.iter() {
                obj.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            serde_json::Value::Object(obj).to_string()
        }
        // Stylesheet body for a link that entered the live document (or
        // re-href'd inside it) after navigation — fetched by the bootstrap's
        // dynamic-link loader and parked here so both layout_run_all's
        // cascade join and document.styleSheets see it. The layout cache
        // must drop: a new sheet can flip any rule match, which the tree
        // epoch cannot express (same reasoning as the attribute-write
        // commands at the top of this dispatcher).
        "ext_sheet_put" => {
            gs.ext_sheets.borrow_mut().insert(arg1, arg2);
            #[cfg(feature = "screenshot")]
            {
                gs.drop_layout();
            }
            "null".to_string()
        }
        // Parse CSS text into rule records for CSSOM: real selector +
        // declaration text from the same parser the cascade uses, so
        // sheet.cssRules agrees with what layout actually applies.
        "parse_css_rules" => {
            let rules = crate::diting_css::parse_stylesheet(&arg1);
            let arr: Vec<serde_json::Value> = rules
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "selectorText": r.selector,
                        "cssText": format!("{}{{{}}}", r.selector, r.declarations),
                        "declarations": r.declarations,
                    })
                })
                .collect();
            serde_json::Value::Array(arr).to_string()
        }
        // Real layout geometry for one element, from the diting_css +
        // diting_layout pipeline. Memoized per tree epoch; a stale epoch
        // (any node allocation/free since the last run) re-lays-out. Returns
        // "[x,y,width,height]" or "null". Gated behind the screenshot
        // feature because diting_layout pulls taffy/swash; without it the
        // bootstrap falls back to its synthetic hit-test grid.
        #[cfg(feature = "screenshot")]
        "layout_rect" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let rect = gs.layout_cache.borrow().as_ref().and_then(|(e, (m, ..))| {
                if *e == epoch { m.get(&nid).copied() } else { None }
            });
            match rect {
                // Sticky v1: the border-box rect carries the scroll-dependent
                // shift so gBCR/offsetTop track the visual position (the
                // same in-flow + sticky pairing the paint side applies).
                // Plain elements read a zero entry — bytes unchanged.
                Some([x, y, w, h]) => {
                    let sh = *sticky_shifts(&gs, dom).get(&nid).unwrap_or(&[0.0, 0.0]);
                    format!("[{},{},{},{}]", x + sh[0], y + sh[1], w, h)
                }
                None => {
                    // A valid layout run for this epoch says the element has
                    // NO box (display:none, detached, or composed-tree-hidden:
                    // an unslotted light child / a slot element) — Chrome
                    // answers an all-zero DOMRect there. "null" stays reserved
                    // for an invalid nid or a failed layout run, the cases
                    // where bootstrap's synthetic grid is the fallback.
                    if gs
                        .layout_cache
                        .borrow()
                        .as_ref()
                        .is_some_and(|(e, _)| *e == epoch)
                    {
                        "[0,0,0,0]".into()
                    } else {
                        "null".into()
                    }
                }
            }
        }
        // Elements in paint order (ascending), from the same layout run as
        // `layout_rect`. Backs elementFromPoint: document order is not paint
        // order once positioned z-index siblings hoist out of it (obscura
        // #738). Boxless inline wrappers are absent — the JS side ranks
        // them with their nearest boxed ancestor.
        #[cfg(feature = "screenshot")]
        "paint_order" => {
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let order = gs.layout_cache.borrow().as_ref().and_then(|(e, (_, o, ..))| {
                if *e == epoch { Some(o.clone()) } else { None }
            });
            let order = order.unwrap_or_default();
            let mut s = String::with_capacity(order.len() * 8 + 2);
            s.push('[');
            for (i, id) in order.iter().enumerate() {
                if i > 0 { s.push(','); }
                s.push_str(&id.index().to_string());
            }
            s.push(']');
            s
        }
        // Per-element LOCAL geometry (blitz #663 family): the pre-map
        // border box and the element's TOTAL accumulated paint map as
        // "[x,y,w,h,a,b,c,d,e,f]", or "null" for a boxless element or a
        // failed run. Backs offsetX/Y inverse mapping and exact-shape hit
        // testing — the JS side rejects hit points that fall inside the
        // element's axis-aligned bounding box but outside the true
        // transformed shape.
        #[cfg(feature = "screenshot")]
        "local_geom" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let guard = gs.layout_cache.borrow();
            let geom = guard.as_ref().and_then(|(e, run)| {
                if *e == epoch { run.4.get(&nid).copied() } else { None }
            });
            drop(guard);
            match geom {
                // Sticky v1: shift the border box and prepend the shift onto
                // the TOTAL map's e/f (T·M — a document-space translate, the
                // same fold the paint side composes into a bracket), so
                // offset inverse-mapping and exact-shape hit tests land on
                // the visual geometry.
                Some(([x, y, w, h], [a, b, c, d, e, f])) => {
                    let sh = *sticky_shifts(&gs, dom).get(&nid).unwrap_or(&[0.0, 0.0]);
                    format!(
                        "[{},{},{},{},{},{},{},{},{},{}]",
                        x + sh[0],
                        y + sh[1],
                        w,
                        h,
                        a,
                        b,
                        c,
                        d,
                        e + sh[0],
                        f + sh[1]
                    )
                }
                None => "null".into(),
            }
        }
        // getBoundingClientRect / getClientRects inside a fabricated iframe
        // document (obscura #976): a sync-created contentDocument is an
        // orphan Rust DOM subtree the main run's rect map never covers, so
        // the main-document `layout_rect` answered all zeros. This sub-run
        // answers in the IFRAME'S OWN viewport (the 300x150 default box the
        // fabricated _IframeWindow publishes), matching Chrome — which
        // measures iframe-doc elements against the iframe's viewport with no
        // coordinate stitching into the host page. arg1 must belong to an
        // orphan subtree; the walk reaching the document node means a
        // main-document element was misrouted here (the JS side only calls
        // this through the iframe-doc marker) and gets "null".
        #[cfg(feature = "screenshot")]
        "iframe_layout_rect" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let mut root = nid;
            while let Some(p) = dom.get_node(root).and_then(|n| n.parent) {
                root = p;
            }
            if root == dom.document() {
                return "null".into();
            }
            let run = iframe_layout_run(&gs, dom, root);
            match run.0.get(&nid) {
                Some(&[x, y, w, h]) => format!("[{},{},{},{}]", x, y, w, h),
                // A fresh run without the nid = boxless element in the iframe
                // doc (display:none, …) — the same all-zero DOMRect contract
                // the main-document `layout_rect` serves.
                None => "[0,0,0,0]".into(),
            }
        }
        // The event-coordinate surface (offsetX/Y inverse mapping, exact
        // hit shapes) inside a fabricated iframe document — same 10-tuple
        // wire format as `local_geom`, served from the iframe-subtree run.
        #[cfg(feature = "screenshot")]
        "iframe_local_geom" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let mut root = nid;
            while let Some(p) = dom.get_node(root).and_then(|n| n.parent) {
                root = p;
            }
            if root == dom.document() {
                return "null".into();
            }
            let run = iframe_layout_run(&gs, dom, root);
            match run.4.get(&nid) {
                Some(&([x, y, w, h], [a, b, c, d, e, f])) => {
                    format!("[{},{},{},{},{},{},{},{},{},{}]", x, y, w, h, a, b, c, d, e, f)
                }
                None => "null".into(),
            }
        }
        // offsetX/offsetY for a hit at a document-space point (arg2
        // "docX,docY" — client + scroll) on element arg1 (blitz #663
        // family): the point inverse-maps through the element's TOTAL
        // accumulated map into its local space, minus the padding-edge
        // origin. Singular or non-finite maps answer "null"; the JS side
        // keeps its constructor defaults there.
        #[cfg(feature = "screenshot")]
        "event_offset" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let Some((px, py)) = arg2.split_once(',').and_then(|(xs, ys)| {
                let x = xs.trim().parse::<f32>().ok()?;
                let y = ys.trim().parse::<f32>().ok()?;
                Some((x, y))
            }) else {
                return "null".into();
            };
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let guard = gs.layout_cache.borrow();
            let Some((_, run)) = guard.as_ref().filter(|(e, _)| *e == epoch) else {
                return "null".into();
            };
            let Some(&([bx, by, _, _], [a, b, c, d, ex, ey])) = run.4.get(&nid) else {
                return "null".into();
            };
            let det = a * d - b * c;
            if det.abs() < 1e-9 {
                return "null".into();
            }
            let lx = (d * (px - ex) - c * (py - ey)) / det;
            let ly = (a * (py - ey) - b * (px - ex)) / det;
            if !lx.is_finite() || !ly.is_finite() {
                return "null".into();
            }
            // Padding edge = border box origin + border + padding on each
            // axis. Px dominates click targets; percent/calc sides answer
            // 0 there (v1 boundary — those elements keep working, just
            // without the sub-box nudge).
            let edge = |len: &Option<crate::diting_css::Length>| match len {
                Some(crate::diting_css::Length::Px(v)) => *v,
                _ => 0.0,
            };
            if let Some(style) = run.2.get(&nid) {
                let ox = lx - bx - edge(&style.border_width.left) - edge(&style.padding.left);
                let oy = ly - by - edge(&style.border_width.top) - edge(&style.padding.top);
                format!("[{},{}]", ox, oy)
            } else {
                format!("[{},{}]", lx - bx, ly - by)
            }
        }
        // CSSOM scrollWidth/scrollHeight for one element as "[w,h]", from
        // the same layout run as `layout_rect`: the element's own box
        // unioned with every laid-out DOM descendant's overflow extent
        // relative to the element's origin (blitz #444 — scroll ranges that
        // ignore overflowing content clamp agents out of the bottom of the
        // page). html/body additionally clamp up to the viewport: Chrome's
        // scrolling area is never smaller than the window, while its
        // client* stay the viewport contract. Element boxes carry the
        // content extent (blocks include their text, abspos kids have their
        // own rects), so the element-rect walk approximates the CSSOM
        // scrollable overflow region without per-margin-box geometry.
        #[cfg(feature = "screenshot")]
        "scroll_extent" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let guard = gs.layout_cache.borrow();
            let Some((_, (rects, _, styles, items, _, _, _))) = guard.as_ref().filter(|(e, _)| *e == epoch)
            else {
                return "null".into();
            };
            let Some(&[ox, oy, ow, oh]) = rects.get(&nid) else { return "null".into() };
            let is_root = dom
                .with_node(nid, |n| {
                    n.as_element().map(|e| {
                        matches!(
                            e.local.to_ascii_lowercase().as_ref(),
                            "html" | "body"
                        )
                    })
                })
                .flatten()
                .unwrap_or(false);
            let mut max_w = ow;
            let mut max_h = oh;
            let mut stack = dom.children(nid);
            while let Some(cur) = stack.pop() {
                // blitz#841: a viewport-fixed box never contributes to any
                // scroller's overflow — its containing block is the viewport,
                // so its (possibly transformed) rect must stay out of the
                // union. The whole subtree skips: everything under it is
                // pinned with it.
                if is_viewport_fixed(dom, styles, cur, nid) {
                    continue;
                }
                stack.extend(dom.children(cur));
                if let Some(&[x, y, w, h]) = rects.get(&cur) {
                    max_w = max_w.max((x + w - ox).max(0.0));
                    max_h = max_h.max((y + h - oy).max(0.0));
                }
            }
            if is_root {
                let (vw, vh) = gs.viewport;
                max_w = max_w.max(vw);
                max_h = max_h.max(vh);
                // The element walk can't see bare text — html/body stretch
                // to the viewport, so a text-only body's ink never lifts the
                // union past vh and the page can't scroll past its first
                // screenful. Fold the Text items' wrap-model extent in (the
                // same fold `band_frame` applies, so JS scrollHeight and the
                // pump's clamp agree on the range).
                let (ink_w, ink_h) = crate::diting_layout::paint::text_ink_extent(items);
                max_w = max_w.max(ink_w);
                max_h = max_h.max(ink_h);
            }
            // Viewport overflow propagation (blitz#880, css-overflow-3
            // §3.3): hidden/clip carried by the root element — its own or
            // handed up by the first body child — makes the viewport itself
            // unscrollable, so the scrolling area collapses to exactly the
            // viewport (Chrome: scrollingElement.scrollHeight ==
            // clientHeight there). Only the root-element query collapses;
            // body keeps its content extent (its used overflow became
            // `visible`). Same clamp `band_frame` applies, so the JS range
            // and the pump's stay in agreement.
            let is_html = dom
                .with_node(nid, |n| {
                    n.as_element()
                        .map(|e| e.local.to_ascii_lowercase().as_ref() == "html")
                })
                .flatten()
                .unwrap_or(false);
            if is_html {
                let eff = crate::diting_layout::effective_viewport_overflow(
                    dom,
                    styles,
                    nid,
                    |id| rects.contains_key(&id),
                );
                if matches!(
                    eff,
                    crate::diting_css::Overflow::Hidden | crate::diting_css::Overflow::Clip
                ) {
                    let (vw, vh) = gs.viewport;
                    max_w = vw;
                    max_h = vh;
                }
            }
            format!("[{},{}]", max_w, max_h)
        }
        // Sticky v2: gate for the bootstrap's scrollTop/scrollLeft setters —
        // is this element a real scroll container the shift walk will
        // honor? "1"/"0"; bare builds answer "null" (falls through to the
        // catch-all below) and the JS side treats that as not-a-scroller.
        #[cfg(feature = "screenshot")]
        "is_scroll_container" => {
            let Some(nid) = parse_nid(&arg1) else { return "0".into() };
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let guard = gs.layout_cache.borrow();
            let Some((_, (rects, _, styles, _, _, _, _))) =
                guard.as_ref().filter(|(e, _)| *e == epoch)
            else {
                return "0".into();
            };
            if is_element_scroller(dom, rects, styles, nid) {
                "1".into()
            } else {
                "0".into()
            }
        }
        // Sticky v2 write-through: record one element scroller's offset.
        // arg1 = nid, arg2 = "left\0top"; the JS setter clamped to the
        // scrollable range before calling. scroll_gen (not the epoch)
        // invalidates the shift caches, so scrolling never re-lays-out.
        #[cfg(feature = "screenshot")]
        "set_node_scroll" => {
            let Some(nid) = parse_nid(&arg1) else { return "ok".into() };
            if let Some((x, y)) = arg2.split_once('\0').and_then(|(x, y)| {
                Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?))
            }) {
                if x.is_finite() && y.is_finite() {
                    dom.set_node_scroll(nid, x, y);
                }
            }
            "ok".into()
        }
        // Cascaded computed values for one element as a single JSON object,
        // from the same style+layout run as layout_rect — the layer
        // getComputedStyle was missing: only inline styles were consulted,
        // so values from a <style> block (z-index, position, …) read back
        // as the initial value (obscura #738's companion trap). Snapshot
        // shape follows upstream's op_computed_style: one call returns the
        // whole table, so a style object costs one native round-trip
        // instead of one per property. Covers the layout-decision
        // properties the ComputedStyle carries (inline style attr is
        // folded in by compute_styles); unknown properties stay absent and
        // the JS caller falls through to its own tables. Used value for
        // width/height stays the bounding rect's job, so those are
        // deliberately not in the table.
        #[cfg(feature = "screenshot")]
        "computed_style" => {
            let nid = match parse_nid(&arg1) { Some(id) => id, None => return "null".into() };
            // Pseudo-element face (batch 102): arg2 routes to the host's
            // cascaded ::before/::after styles. The JS wrapper validates the
            // argument (TypeError for anything not `::name` or a legacy
            // single-colon form), so anything non-empty reaching here answers
            // either a pseudo cascade or — for pseudos with no matching rule
            // and for `::name` forms the engine doesn't model — an
            // initial-value table, matching Chrome's no-throw posture.
            let pseudo_kind = match arg2.trim().to_ascii_lowercase().as_str() {
                "" => 0u8,
                "::before" | ":before" => 1,
                "::after" | ":after" => 2,
                _ => 3,
            };
            let cssom_tag = dom
                .with_node(nid, |n| {
                    n.as_element().map(|e| e.local.as_ref().to_ascii_lowercase())
                })
                .flatten();
            let epoch = dom.epoch();
            ensure_layout_run(&gs, dom, epoch);
            let style = gs.layout_cache.borrow().as_ref().and_then(|(e, (_, _, s, _, _, _, _))| {
                if *e == epoch { s.get(&nid).cloned() } else { None }
            });
            match style {
                Some(s) => {
                    // The pseudo cascade lives on the host's ComputedStyle;
                    // a pseudo with no rule serializes the initial-value
                    // table rather than an empty declaration.
                    let pseudo_style = match pseudo_kind {
                        1 => s.pseudos.as_ref().and_then(|p| p.before.as_ref()).cloned(),
                        2 => s.pseudos.as_ref().and_then(|p| p.after.as_ref()).cloned(),
                        _ => None,
                    };
                    let (target, tag) = match pseudo_style {
                        Some(ps) => (ps, None),
                        None if pseudo_kind == 0 => (s, cssom_tag),
                        None => (crate::diting_css::ComputedStyle::default(), None),
                    };
                    let mut obj = serde_json::Map::with_capacity(
                        COMPUTED_STYLE_PROPS.len() + target.custom.len(),
                    );
                    for prop in COMPUTED_STYLE_PROPS {
                        if let Some(v) = computed_style_value(&target, prop, tag.as_deref()) {
                            obj.insert((*prop).to_string(), serde_json::Value::String(v));
                        }
                    }
                    // Custom properties ride the same snapshot (case-sensitive
                    // keys — the JS lookup skips its kebab-lowercase step for
                    // `--` names): getComputedStyle(el).getPropertyValue('--x').
                    for (k, v) in &target.custom {
                        obj.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                    serde_json::Value::Object(obj).to_string()
                }
                None => "null".into(),
            }
        }
        #[cfg(not(feature = "screenshot"))]
        "computed_style" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "layout_rect" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "paint_order" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "scroll_extent" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "local_geom" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "event_offset" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "iframe_layout_rect" => "null".into(),
        #[cfg(not(feature = "screenshot"))]
        "iframe_local_geom" => "null".into(),
        _ => "null".into(),
    }
}

/// Property names declared in an inline style string, lowercased. A
/// fragment without a colon (malformed declaration, comment) counts as its
/// raw text so garbage can never whitelist by accident — it simply never
/// matches {transform, opacity}.
#[cfg(feature = "screenshot")]
fn style_property_names(style: &str) -> std::collections::HashSet<String> {
    style
        .split(';')
        .map(|decl| decl.split_once(':').map(|(name, _)| name).unwrap_or(decl).trim())
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Is a style-attribute write paint-only — every property name it ADDS or
/// REMOVES sits inside {transform, opacity}? Values may differ freely: the
/// taffy solve reads neither property, and the collect walk re-reads both
/// from fresh computed styles, so only the name diff matters for geometry.
/// A missing side (no prior attribute to diff against) answers false — an
/// unmeasurable before-state gets the full invalidation, never a guessed
/// cache reuse.
#[cfg(feature = "screenshot")]
fn style_write_is_paint_only(old: Option<&str>, new: Option<&str>) -> bool {
    let (Some(old), Some(new)) = (old, new) else { return false };
    let old = style_property_names(old);
    let new = style_property_names(new);
    old.symmetric_difference(&new)
        .all(|name| name == "transform" || name == "opacity")
}

/// Can an attribute write never change computed style or layout on its own?
/// The inert set is focus/a11y/script hooks (obscura#983); the escape hatch
/// is the rule pool — a stylesheet selecting on the name (`[data-x]`)
/// disqualifies it. `None` for the pool means no layout run has happened
/// yet, so no cache exists to invalidate and skipping is free.
#[cfg(feature = "screenshot")]
fn attr_write_is_layout_inert(
    name: &str,
    selector_names: Option<&std::collections::HashSet<String>>,
) -> bool {
    let n = name.to_ascii_lowercase();
    let inert = matches!(
        n.as_str(),
        "tabindex" | "title" | "role" | "accesskey" | "draggable" | "spellcheck"
    ) || n.starts_with("data-")
        || n.starts_with("aria-");
    inert && !selector_names.is_some_and(|s| s.contains(n.as_str()))
}

/// One style+layout run over the live tree (see [`JsState::layout_cache`]):
/// every element's border-box rect, the paint order, the cascaded
/// ComputedStyle per element, the flat paint-item list in paint order
/// (band paint's input — computed anyway, so caching it is free), and the
/// per-element LOCAL geometry pair (pre-map border box + total accumulated
/// map) backing the event-coordinate surface (`local_geom`/`event_offset`
/// ops, blitz #663 family).
#[cfg(feature = "screenshot")]
type LayoutRun = (
    HashMap<NodeId, [f32; 4]>,
    Vec<NodeId>,
    HashMap<NodeId, crate::diting_css::ComputedStyle>,
    Vec<crate::diting_layout::PaintItem>,
    HashMap<NodeId, ([f32; 4], [f32; 6])>,
    // Sticky subtree item spans (root-scroller v1): (node, start, end) into
    // the items vec — see layout_collect. Readers pair them with the
    // scroll-dependent shift map (sticky_shifts).
    Vec<crate::diting_layout::StickySpan>,
    // Scroller subtree item spans (sticky v2): same shape, one per real
    // scroll container, recorded after its Clip and closed before its
    // PopClip — the read-time shift walk translates the span when the
    // container's scrollTop/scrollLeft is non-zero.
    Vec<crate::diting_layout::StickySpan>,
);

/// Run the full diting style + layout pipeline over the live DOM tree and
/// return every element's border-box rect, the paint order (see
/// `layout_dom_with_paint_order_and_images`) and the cascaded ComputedStyle
/// per element (for the `computed_style` op). Styles are re-collected each
/// run: attribute-level mutations (style/class writes) don't bump the tree
/// epoch, so memoizing computed styles alongside the rects would serve
/// stale geometry.
#[cfg(feature = "screenshot")]
fn layout_run_all(gs: &JsState, dom: &DomTree) -> LayoutRun {
    // Debug knob (AGINXBROWSER_LAYOUT_TRACE=1): phase timings for a full
    // layout run. The interrupt-sampler evidence on the WeChat article pages
    // shows multi-second stretches with no V8 stack check — candidates are
    // the Rust phases below, each re-done from scratch on every epoch bump.
    let trace = std::env::var("AGINXBROWSER_LAYOUT_TRACE").is_ok();
    let t0 = std::time::Instant::now();
    // Same viewport the persona publishes to window.innerWidth/innerHeight,
    // so geometry agrees with what scripts read off `window` (and the ICB
    // has a definite size for fixed-box inset resolution — obscura#675).
    let (viewport_width, viewport_height) = gs.viewport;
    let mut css = String::new();
    // Sheets join in DOCUMENT order (a comma query returns exactly that):
    // an earlier <link> loses equal-specificity ties to a later <style>,
    // like Chrome's cascade. External sheet bodies come from the
    // navigation-time fetch table (JsState::ext_sheets); a <link> whose
    // fetch failed (or was blocked) contributes nothing, matching a
    // browser that leaves the sheet empty rather than dropping the page's
    // other styling.
    if let Ok(els) = dom.query_selector_all("style, link") {
        for el in els {
            let meta = dom.with_node(el, |n| {
                let tag = n.as_element()?.local.to_ascii_lowercase();
                Some((
                    tag,
                    n.get_attribute("rel").map(|v| v.to_ascii_lowercase()),
                    n.get_attribute("href").map(|v| v.to_string()),
                ))
            });
            let Some((tag, rel, href)) = meta.flatten() else { continue };
            match &*tag {
                "style" => {
                    css.push_str(&dom.text_content(el));
                    css.push('\n');
                }
                "link" => {
                    let is_sheet = rel
                        .as_deref()
                        .is_some_and(|r| r.split_ascii_whitespace().any(|t| t == "stylesheet"));
                    if !is_sheet {
                        continue;
                    }
                    let Some(href) = href.filter(|h| !h.is_empty()) else { continue };
                    let abs = Url::parse(&gs.url)
                        .ok()
                        .and_then(|base| base.join(&href).ok())
                        .map(|u| u.to_string());
                    let Some(abs) = abs else { continue };
                    if let Some(body) = gs.ext_sheets.borrow().get(&abs) {
                        css.push_str(body);
                        css.push('\n');
                    }
                }
                _ => {}
            }
        }
    }
    // Shadow trees are invisible to the document-rooted query above, but
    // their <style> blocks style shadow content (composed rendering, phase
    // 2). They join the pool after the light sheets — same global-pool
    // approximation the rule matcher uses.
    for text in crate::diting_layout::shadow_style_texts(dom) {
        css.push_str(&text);
        css.push('\n');
    }
    let t_css = t0.elapsed();
    let (mut rules, keyframes, containers) = crate::diting_css::parse_stylesheet_full(
        &css,
        (viewport_width, viewport_height),
        gs.media_type,
        &gs.media_overrides,
    );
    let t_parse = t0.elapsed();
    let mut styles_map = crate::diting_layout::compute_styles_timed(
        dom,
        &rules,
        &keyframes,
        gs.css_time,
        &gs.css_transitions.borrow(),
    );
    // @container stage (moli#282): conditions answer against ancestor
    // container geometry, which only exists after a solve — so the arms
    // cascade in a second gated pass. Pass 1 styled the base rules; probe
    // boxes come from the cached solve when it is fresh (any previous run's
    // final geometry is a fine container-size estimate) and from one extra
    // solve otherwise. Pages without @container skip all of this.
    let mut styles_bumped = false;
    if !containers.is_empty() {
        let base_len = rules.len();
        let probe_boxes = match gs.geometry_cache.borrow().as_ref() {
            Some((e, solved)) if *e == dom.epoch() => solved.dom_boxes(),
            _ => {
                let fonts_probe = crate::diting_fonts::font_book();
                let bytes_map = gs.image_bytes.borrow();
                let network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>> =
                    if bytes_map.is_empty() { None } else { Some(&bytes_map) };
                let probe = crate::diting_layout::layout_solve(
                    dom,
                    &styles_map,
                    &fonts_probe,
                    viewport_width,
                    viewport_height,
                    network_bytes,
                    Some(gs.url.as_str()),
                );
                probe.dom_boxes()
            }
        };
        let plan = crate::diting_layout::container_plan(
            dom,
            &styles_map,
            &probe_boxes,
            &containers,
            base_len,
        );
        if !plan.extra_rules.is_empty() {
            rules.extend(plan.extra_rules);
            styles_map = crate::diting_layout::compute_styles_gated(
                dom,
                &rules,
                &keyframes,
                gs.css_time,
                &gs.css_transitions.borrow(),
                base_len,
                &plan.gates,
            );
            styles_bumped = true;
        }
    }
    // Refresh the attribute-selector name pool the write path consults for
    // inert-attribute invalidation skips (obscura#983). Sits after the
    // container stage so @container inner rules join the pool too.
    *gs.attr_selector_names.borrow_mut() =
        Some(crate::diting_css::collect_selector_attr_names(&rules));
    let t_styles = t0.elapsed();
    // Finite animations span delay + duration * iterations. An endless one
    // can't pin a length on its own, so it contributes nothing unless it is
    // the only animation — then one full cycle keeps the extent the
    // single-cycle sampler used to report (a -t pin overrides anyway).
    // Registered transitions join the extent: a clip must run long enough
    // to show them finish.
    let mut css_extent = 0.0f64;
    let mut endless_cycle = 0.0f64;
    for a in styles_map.values().filter_map(|cs| cs.animation.as_ref()) {
        if a.iterations.is_finite() {
            css_extent = css_extent.max((a.delay + a.duration * a.iterations) as f64);
        } else {
            endless_cycle = endless_cycle.max((a.delay + a.duration) as f64);
        }
    }
    for tr in gs.css_transitions.borrow().iter() {
        css_extent = css_extent.max(tr.start + (tr.delay + tr.duration) as f64);
    }
    if css_extent <= 0.0 {
        css_extent = endless_cycle;
    }
    gs.css_extent.set(css_extent);
    let fonts = crate::diting_fonts::font_book();
    // Solve-vs-collect split (#395): the taffy solve is cached keyed by the
    // tree epoch. A paint-only style write (transform/opacity — the choke
    // point in op_dom_inner) dropped the collected cache but kept this one,
    // so the run below re-collects against the cached tree. Anything
    // structural (tree mutation, class change, image bytes landing,
    // viewport move) drops both caches and re-solves.
    let epoch = dom.epoch();
    // A gated container re-cascade changed styles without bumping the epoch:
    // the cached solve (if any) was built against pass-1 styles, so it must
    // not be reused — force the fresh solve against the final cascade.
    let reuse = !styles_bumped
        && gs
            .geometry_cache
            .borrow()
            .as_ref()
            .is_some_and(|(e, _)| *e == epoch);
    let solve_src = if reuse { "cached" } else { "full" };
    let (rects, items, paint_order, local_geom, sticky_spans, scroller_spans) = if reuse {
        // Borrow held only across layout_collect, which never touches
        // JsState — nothing else can interleave on this single thread.
        let guard = gs.geometry_cache.borrow();
        let (_, solved) = guard.as_ref().expect("freshness checked above");
        crate::diting_layout::layout_collect(dom, &styles_map, &fonts, solved, viewport_width)
    } else {
        // The byte table the run resolves http(s) img sources against.
        // Empty → None keeps the all-placeholder path byte-identical to
        // before (and lets the caller decide when rasters are worth
        // fetching).
        let bytes_map = gs.image_bytes.borrow();
        let network_bytes: Option<&HashMap<String, std::sync::Arc<Vec<u8>>>> =
            if bytes_map.is_empty() { None } else { Some(&bytes_map) };
        let solved = crate::diting_layout::layout_solve(
            dom,
            &styles_map,
            &fonts,
            viewport_width,
            viewport_height,
            network_bytes,
            Some(gs.url.as_str()),
        );
        drop(bytes_map);
        gs.solves.set(gs.solves.get() + 1);
        let run = crate::diting_layout::layout_collect(
            dom,
            &styles_map,
            &fonts,
            &solved,
            viewport_width,
        );
        *gs.geometry_cache.borrow_mut() = Some((epoch, solved));
        run
    };
    if trace {
        let t_layout = t0.elapsed();
        eprintln!(
            "[layout-trace] total={:?} solve={} css_collect={:?} css_parse={:?} compute_styles={:?} layout={:?} css_bytes={} rects={}",
            t_layout,
            solve_src,
            t_css,
            t_parse - t_css,
            t_styles - t_parse,
            t_layout - t_styles,
            css.len(),
            rects.len(),
        );
    }
    (
        rects.into_iter().map(|(id, r)| (id, [r.x, r.y, r.width, r.height])).collect(),
        paint_order,
        styles_map,
        items,
        local_geom
            .into_iter()
            .map(|(id, (r, m))| {
                (id, ([r.x, r.y, r.width, r.height], m))
            })
            .collect(),
        sticky_spans,
        scroller_spans,
    )
}

/// Run one layout pass only when the memoized run is stale for the tree's
/// current epoch; callers then read their slice from the (now fresh) cache.
///
/// Freshness must be checked separately from membership: a nid absent from
/// the rects map is a legitimate miss on a FRESH cache (boxless element —
/// <head>, display:none, svg children under the svg v1 one-box model).
/// Treating "absent" as "stale" re-ran the whole page layout per boxless
/// lookup; on svg-heavy pages (archify artifacts: ~530 boxless svg nodes) a
/// single elementFromPoint walk re-laid-out the page hundreds of times and
/// tripped the 10s eval watchdog — surfacing as silent `null` eval results
/// and dead clicks.
#[cfg(feature = "screenshot")]
fn ensure_layout_run(gs: &JsState, dom: &DomTree, epoch: u64) {
    let stale = gs.layout_cache.borrow().as_ref().map(|(e, _)| *e == epoch) != Some(true);
    if stale {
        let run = layout_run_all(gs, dom);
        *gs.layout_cache.borrow_mut() = Some((epoch, run));
    }
}

/// One style+layout run over an ORPHAN SUBTREE — a fabricated iframe
/// document (obscura #976 family), memoized per (epoch, root) in
/// [`JsState::iframe_layout_cache`]. The sub-run anchors to the 300x150
/// default box the fabricated `_IframeWindow` publishes (the iframe's own
/// viewport, per Chrome's gBCR semantics) and never touches the main page's
/// layout caches. Inline <style> sheets only (v1 boundary): external <link>
/// sheets inside a fabricated iframe doc would need the navigation-time
/// fetch cascade the main document owns.
#[cfg(feature = "screenshot")]
fn iframe_layout_run(gs: &JsState, dom: &DomTree, root: NodeId) -> std::rc::Rc<LayoutRun> {
    let epoch = dom.epoch();
    if let Some(hit) = gs.iframe_layout_cache.borrow().as_ref().and_then(|(e, r, run)| {
        (*e == epoch && *r == root).then(|| std::rc::Rc::clone(run))
    }) {
        return hit;
    }
    let mut css = String::new();
    let mut stack = dom.children(root);
    while let Some(cur) = stack.pop() {
        stack.extend(dom.children(cur));
        let is_style = dom
            .with_node(cur, |n| {
                n.as_element().map(|e| *e.local.to_ascii_lowercase() == *"style")
            })
            .flatten()
            .unwrap_or(false);
        if is_style {
            css.push_str(&dom.text_content(cur));
            css.push('\n');
        }
    }
    const IFRAME_VW: f32 = 300.0;
    const IFRAME_VH: f32 = 150.0;
    let (rules, keyframes) = crate::diting_css::parse_stylesheet_timed_with(
        &css,
        (IFRAME_VW, IFRAME_VH),
        gs.media_type,
        &gs.media_overrides,
    );
    let styles_map = crate::diting_layout::compute_styles_timed_within(
        dom,
        &rules,
        &keyframes,
        None,
        root,
        // Transitions are registered against main-document node ids; the
        // subtree id space here is separate, so the empty slice stands.
        &[],
    );
    let fonts = crate::diting_fonts::font_book();
    let solved = crate::diting_layout::layout_solve_rooted(
        dom,
        &styles_map,
        &fonts,
        IFRAME_VW,
        IFRAME_VH,
        None,
        None,
        Some(root),
    );
    let (rects, items, paint_order, local_geom, sticky_spans, scroller_spans) =
        crate::diting_layout::layout_collect(dom, &styles_map, &fonts, &solved, IFRAME_VW);
    let run = std::rc::Rc::new((
        rects
            .into_iter()
            .map(|(id, r)| (id, [r.x, r.y, r.width, r.height]))
            .collect(),
        paint_order,
        styles_map,
        items,
        local_geom
            .into_iter()
            .map(|(id, (r, m))| (id, ([r.x, r.y, r.width, r.height], m)))
            .collect(),
        sticky_spans,
        scroller_spans,
    ));
    *gs.iframe_layout_cache.borrow_mut() = Some((epoch, root, std::rc::Rc::clone(&run)));
    run
}

/// Whether `node` is a position:fixed box whose containing block is the
/// VIEWPORT — no ancestor between it and `root` carries a transform. CSS
/// transforms establish a new containing block for fixed descendants, so a
/// fixed box under a transformed ancestor re-anchors into that subtree and
/// DOES contribute to its scrollable overflow; only the viewport-anchored
/// kind is exempt (blitz#841: upstream's viewport became scrollable because
/// a translated fixed element entered the overflow walk).
#[cfg(feature = "screenshot")]
fn is_viewport_fixed(
    dom: &DomTree,
    styles: &HashMap<NodeId, crate::diting_css::ComputedStyle>,
    node: NodeId,
    root: NodeId,
) -> bool {
    if !styles
        .get(&node)
        .is_some_and(|st| st.position == Some(crate::diting_css::PositionMode::Fixed))
    {
        return false;
    }
    let mut cur = dom.get_node(node).and_then(|n| n.parent);
    while let Some(id) = cur {
        if id == root {
            return true;
        }
        if styles.get(&id).is_some_and(|st| st.transform.is_some()) {
            return false;
        }
        cur = dom.get_node(id).and_then(|n| n.parent);
    }
    true
}

/// A real element scroll container for sticky v2: boxed, clipping,
/// scrollable used overflow (hidden IS a scroll container per css-overflow,
/// clip is not), and not one the viewport owns by propagation — html's
/// overflow always propagates to the viewport, body's when html is visible
/// (body_overflow_propagates, the same predicate the clip-emission walk
/// used, so the walks agree on what the viewport owns). Shared by the
/// `is_scroll_container` op and the sticky_shifts source filter: what JS
/// may write and what paint consumes cannot drift apart.
#[cfg(feature = "screenshot")]
fn is_element_scroller(
    dom: &DomTree,
    rects: &HashMap<NodeId, [f32; 4]>,
    styles: &HashMap<NodeId, crate::diting_css::ComputedStyle>,
    id: NodeId,
) -> bool {
    if !rects.contains_key(&id) {
        return false;
    }
    let Some(s) = styles.get(&id) else { return false };
    if !(s.clips_descendants() && s.effective_overflow() != crate::diting_css::Overflow::Clip) {
        return false;
    }
    let tag = dom
        .with_node(id, |n| {
            n.as_element().map(|e| e.local.to_ascii_lowercase().to_string())
        })
        .flatten();
    match tag.as_deref() {
        Some("html") => false,
        Some("body") => !crate::diting_layout::body_overflow_propagates(
            dom,
            styles,
            id,
            |x| rects.contains_key(&x),
        ),
        _ => true,
    }
}

/// The sticky shift map for the CURRENT read (sticky v2): one [dx, dy]
/// per node inside a shifted subtree — sticky boxes AND element-scroller
/// contents. A sticky's value is its constraint shift composed over
/// enclosing sources; a scroller's descendants carry the negated scroll
/// offset the same way. Constraints are computed HERE, not at layout
/// time: they depend on the scroll offsets, which are orthogonal to the
/// layout epoch (scroll-blind layout — the same invariant that lets the
/// band pump re-blit without re-solving). Memoized per (epoch,
/// layout_rev, root scroll, viewport, scroll_gen) in
/// [`JsState::sticky_shift_cache`].
///
/// Sources — sticky nodes and scrolled element scrollers — are processed
/// SHALLOW-FIRST with one composition rule: a source's total is its own
/// contribution plus whatever shallower sources already wrote onto it
/// (out[nid] IS the nearest enclosing source's total, because every
/// shallower subtree write covered this node). Each source then writes
/// its folded total across its subtree, which composes
/// sticky-in-scroller, scroller-in-sticky, and a node that is BOTH — its
/// sticky branch runs first (covers self), its scroller branch bases on
/// that and rewrites only the descendants, exactly the two nested paint
/// spans its element records.
///
/// Gates (documented Chrome divergences, each degrades to plain relative
/// or unscrolled paint):
/// - A source under a position:fixed or transformed ANCESTOR gets no
///   shift: the fixed subtree has no scroller relation, and a
///   transformed ancestor brackets its subtree's paint in local coords,
///   where a uniform document-space translate can't compose from inside
///   the span. This also gates a transformed element scroller and sticky
///   inside one (accepted v2 approximation). The source's OWN transform
///   is fine — its bracket sits inside its span and
///   apply_sticky_to_items folds the shift into it.
/// - Sticky scrollport = the nearest element scroller's padding box plus
///   that scroller's OWN offset (outer scrollers cancel: port and
///   content ride them together); with none, the root scroller /
///   viewport (v1). html/body never count as element scrollers — their
///   overflow propagates to the viewport (css-overflow §3.3;
///   body_overflow_propagates is the same predicate the clip-emission
///   walk uses, so the two walks agree on what the viewport owns).
/// - Containing block = nearest boxed ancestor's border box (Blink uses
///   margin-box/content-box refinements).
/// - Boxless sticky (display:contents, inline wrappers) is skipped.
#[cfg(feature = "screenshot")]
fn sticky_shifts(gs: &JsState, dom: &DomTree) -> std::rc::Rc<HashMap<NodeId, [f32; 2]>> {
    use crate::diting_css::{Length, PositionMode};

    let epoch = dom.epoch();
    let (sx, sy) = gs.scroll_offset;
    let (vw, vh) = gs.viewport;
    // scroll_gen joins the key: element-scroller offsets change WITHOUT an
    // epoch or layout_rev bump (read-time paint state), so without it a
    // scrollTop write would keep serving the stale map.
    let key = (epoch, gs.layout_rev.get(), sx, sy, vw, vh, dom.scroll_gen());
    if let Some((k, v)) = gs.sticky_shift_cache.borrow().as_ref() {
        if *k == key {
            return std::rc::Rc::clone(v);
        }
    }
    ensure_layout_run(gs, dom, epoch);
    let guard = gs.layout_cache.borrow();
    let empty = std::rc::Rc::new(HashMap::new());
    let Some((_, (rects, _, styles, _, _, _, _))) = guard.as_ref().filter(|(e, _)| *e == epoch)
    else {
        return empty;
    };

    // Unified shift sources: (node, is_scroller). Sticky entries are
    // appended before scroller entries, so at equal depth a node that is
    // both runs its sticky branch first — the composition order its two
    // nested paint spans resolve in. NO dedup: dedup would drop the
    // second entry of a both-node and lose its scroll.
    let scrolls = dom.scroll_offsets();
    let mut sources: Vec<(NodeId, bool)> = styles
        .iter()
        .filter(|(_, s)| s.position == Some(PositionMode::Sticky))
        .map(|(n, _)| (*n, false))
        .filter(|(n, _)| rects.contains_key(n))
        .collect();
    sources.extend(
        scrolls
            .iter()
            .filter(|(_n, o)| o[0] != 0.0 || o[1] != 0.0)
            .map(|(n, _)| (*n, true))
            .filter(|(n, _)| is_element_scroller(dom, rects, styles, *n)),
    );
    sources.sort_by_key(|(n, _)| {
        let mut d = 0usize;
        let mut cur = dom.get_node(*n).and_then(|nd| nd.parent);
        while let Some(p) = cur {
            d += 1;
            cur = dom.get_node(p).and_then(|nd| nd.parent);
        }
        d
    });

    let resolve = |l: &Option<Length>, port: f32| {
        l.as_ref()
            .map(|l| crate::diting_layout::sticky_inset_px(l, port))
    };
    // Border widths count only with a border-style — the clip walk's
    // side_px rule; the scrollport is the padding box.
    let side_px = |l: &Option<Length>| match l {
        Some(Length::Px(v)) => *v,
        _ => 0.0,
    };

    let mut out: HashMap<NodeId, [f32; 2]> = HashMap::new();
    for (nid, scroller_source) in sources {
        // One ancestor walk, three uses: gate on fixed/transformed (no
        // shift), first boxed ancestor's border box (the CB), nearest
        // element scroller (the sticky's scrollport). The gate must see
        // the WHOLE chain — a transform above the CB still brackets the
        // paint.
        let mut gated = false;
        let mut cb: Option<(NodeId, [f32; 4])> = None;
        let mut port: Option<([f32; 4], [f32; 2])> = None;
        let mut cur = dom.get_node(nid).and_then(|nd| nd.parent);
        while let Some(p) = cur {
            if let Some(st) = styles.get(&p) {
                if st.position == Some(PositionMode::Fixed) || st.transform.is_some() {
                    gated = true;
                    break;
                }
            }
            if cb.is_none() {
                if let Some(&r) = rects.get(&p) {
                    cb = Some((p, r));
                }
            }
            if port.is_none() && is_element_scroller(dom, rects, styles, p) {
                let [px, py, pw, ph] = rects[&p];
                let st = &styles[&p];
                // Padding box = border box inset by border widths, which
                // apply only with a border-style.
                let (bt, br, bb, bl) = if st.border_style.is_some() {
                    (
                        side_px(&st.border_width.top),
                        side_px(&st.border_width.right),
                        side_px(&st.border_width.bottom),
                        side_px(&st.border_width.left),
                    )
                } else {
                    (0.0, 0.0, 0.0, 0.0)
                };
                port = Some((
                    [
                        px + bl,
                        py + bt,
                        (pw - bl - br).max(0.0),
                        (ph - bt - bb).max(0.0),
                    ],
                    scrolls.get(&p).copied().unwrap_or([0.0, 0.0]),
                ));
            }
            cur = dom.get_node(p).and_then(|nd| nd.parent);
        }
        if gated {
            continue;
        }
        // A scroll container's rect in the map is its CLIP box, but a
        // sticky inside it travels the whole scrollable content — the
        // clamp range is the subtree union, the same extent scrollHeight
        // reports. v1 never noticed: at the root the body box already
        // spans the full document, so union == rect there.
        if let Some((cb_node, cb_rect)) = cb.as_mut() {
            if is_element_scroller(dom, rects, styles, *cb_node) {
                let [bx, by, _, _] = *cb_rect;
                let mut stack = dom.children(*cb_node);
                while let Some(c) = stack.pop() {
                    if let Some(&[rx, ry, rw, rh]) = rects.get(&c) {
                        cb_rect[2] = cb_rect[2].max(rx + rw - bx);
                        cb_rect[3] = cb_rect[3].max(ry + rh - by);
                    }
                    stack.extend(dom.children(c));
                }
            }
        }
        // Base = the total already written on this node by shallower
        // sources; out[nid] is the nearest enclosing source's total
        // because every shallower subtree write covered this node.
        let base = out.get(&nid).copied().unwrap_or([0.0, 0.0]);
        let (own, covers_self) = if scroller_source {
            // Content translates UP by the scroll offset; the scroller's
            // own box/border/clip stay fixed (its span opens after its
            // clip, the map write skips the node itself).
            let o = scrolls.get(&nid).copied().unwrap_or([0.0, 0.0]);
            ([-o[0], -o[1]], false)
        } else {
            let Some(&[x, y, w, h]) = rects.get(&nid) else { continue };
            let Some((_, [cx, cy, cw, ch])) = cb else { continue };
            let st = &styles[&nid];
            // Port: nearest element scroller (v2) with its OWN offset —
            // outer scrollers cancel because the port rides them with the
            // content. No scroller ancestor: the root scroller (v1).
            // (py + oy) against the UNSCROLLED y is the pinned read: the
            // port rides every outer source with the content, so only the
            // port's own scroll distinguishes them.
            let (dy, dx) = if let Some(([px, py, pw, ph], [ox, oy])) = port {
                (
                    crate::diting_layout::sticky_axis_shift(
                        y,
                        h,
                        resolve(&st.top, ph),
                        resolve(&st.bottom, ph),
                        py + oy,
                        ph,
                        cy,
                        cy + ch,
                    ),
                    crate::diting_layout::sticky_axis_shift(
                        x,
                        w,
                        resolve(&st.left, pw),
                        resolve(&st.right, pw),
                        px + ox,
                        pw,
                        cx,
                        cx + cw,
                    ),
                )
            } else {
                (
                    crate::diting_layout::sticky_axis_shift(
                        y,
                        h,
                        resolve(&st.top, vh),
                        resolve(&st.bottom, vh),
                        sy,
                        vh,
                        cy,
                        cy + ch,
                    ),
                    crate::diting_layout::sticky_axis_shift(
                        x,
                        w,
                        resolve(&st.left, vw),
                        resolve(&st.right, vw),
                        sx,
                        vw,
                        cx,
                        cx + cw,
                    ),
                )
            };
            if dx == 0.0 && dy == 0.0 {
                // Still in flow at this scroll — descendants read no base
                // shift from us, and this subtree keeps any ancestor total.
                continue;
            }
            ([dx, dy], true)
        };
        let total = [own[0] + base[0], own[1] + base[1]];
        // Write the total over the subtree (self included for a sticky —
        // its own box moves; excluded for a scroller — only its content
        // does). Every read point (gBCR, hit testing, band paint) shifts
        // uniformly; a deeper source later overwrites its own subtree
        // with its total, which already folded this one via the base
        // lookup above.
        let mut stack = if covers_self {
            vec![nid]
        } else {
            dom.children(nid)
        };
        while let Some(cur) = stack.pop() {
            out.insert(cur, total);
            stack.extend(dom.children(cur));
        }
    }
    drop(guard);
    let rc = std::rc::Rc::new(out);
    *gs.sticky_shift_cache.borrow_mut() = Some((key, std::rc::Rc::clone(&rc)));
    rc
}

/// One viewport-band frame: RGBA pixels plus the scroll offset actually
/// painted and the document's scrollable extent.
#[cfg(feature = "screenshot")]
pub(crate) struct BandFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The scroll offset actually painted — clamped into
    /// `[0, content − viewport]`, so `metadata.scrollOffsetX/Y` report this,
    /// not the raw request.
    pub dx: f32,
    pub dy: f32,
    /// CSS-pixel content size (root scrollWidth×scrollHeight).
    pub content_size: (f32, f32),
    /// PDF text-layer glyph ops for this band, band-local (vector-text
    /// batch). Empty unless the band was painted via `band_frame_with_text`.
    pub text_ops: Vec<crate::diting_layout::paint::PdfOp>,
}

/// Paint the viewport band `[dx, dx+vw) × [dy, dy+vh)` of the live tree's
/// cached layout into a viewport-sized canvas — the CDP viewport-capture /
/// screencast frame path. No outerHTML re-parse and no full-page raster:
/// layout is scroll-blind and memoized per tree epoch, so scrolling is a
/// pure re-blit and per-frame cost is independent of page height (the root
/// fix for the ~1.4s/frame full-page re-render the AginxOS report measured).
///
/// Returns the frame plus the absolute img URLs the page references but
/// [`JsState::image_bytes`] lacks — the async caller fetches them (same
/// per-URL policy as `prefetch_render_resources`: page client, SSRF gate,
/// size/timeout caps) via [`store_image_bytes`] and calls again for the
/// image-complete frame. Band paint itself never touches the network.
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, scroll_x, scroll_y, viewport, false)
}

/// Same band paint but ALSO collects the PDF text layer (vector-text batch):
/// vectorizable `Text` items leave the raster — the PDF layer redraws them as
/// glyphs, and painting twice would double the antialiasing — and come back
/// as band-local glyph ops on the frame.
#[cfg(feature = "screenshot")]
pub(crate) fn band_frame_with_text(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
) -> Option<(BandFrame, Vec<String>)> {
    band_frame_inner(gs, scroll_x, scroll_y, viewport, true)
}

#[cfg(feature = "screenshot")]
fn band_frame_inner(
    gs: &JsState,
    scroll_x: f32,
    scroll_y: f32,
    viewport: (f32, f32),
    collect_text: bool,
) -> Option<(BandFrame, Vec<String>)> {
    gs.band_paints.set(gs.band_paints.get() + 1);
    let dom = gs.dom.as_ref()?;
    // Same page-height cap the full-page render uses — a malicious client
    // requesting a 1e9-viewport must not allocate for it.
    const MAX_BAND: f32 = 16000.0;
    let vw = if viewport.0.is_finite() { viewport.0.max(1.0) } else { 1.0 }.min(MAX_BAND);
    let vh = if viewport.1.is_finite() { viewport.1.max(1.0) } else { 1.0 }.min(MAX_BAND);

    let epoch = dom.epoch();
    ensure_layout_run(gs, dom, epoch);
    let guard = gs.layout_cache.borrow();
    let (_, (rects, _, styles, items, _, sticky_spans, scroller_spans)) =
        guard.as_ref().filter(|(e, _)| *e == epoch)?;

    // Scrollable content extent: the root scroller's box unioned with every
    // laid-out descendant, clamped up to the viewport — the same union the
    // `scroll_extent` op serves the JS side, so the pump's clamp and
    // window.scrollY agree on the range.
    let root = dom
        .query_selector_all("html")
        .ok()
        .and_then(|v| v.into_iter().next())
        .or_else(|| dom.query_selector_all("body").ok().and_then(|v| v.into_iter().next()));
    let mut content_w = vw;
    let mut content_h = vh;
    if let Some(root) = root.filter(|r| rects.contains_key(r)) {
        if let Some(&[ox, oy, ow, oh]) = rects.get(&root) {
            content_w = content_w.max(ow);
            content_h = content_h.max(oh);
            let mut stack = dom.children(root);
            while let Some(cur) = stack.pop() {
                // Same viewport-fixed skip as the scroll_extent op (blitz#841)
                // — the two walks must agree or JS scrollHeight and the
                // pump's clamp disagree on the scroll range.
                if is_viewport_fixed(dom, styles, cur, root) {
                    continue;
                }
                stack.extend(dom.children(cur));
                if let Some(&[x, y, w, h]) = rects.get(&cur) {
                    content_w = content_w.max((x + w - ox).max(0.0));
                    content_h = content_h.max((y + h - oy).max(0.0));
                }
            }
        }
    }
    // The rect union is element-only and the html/body boxes stretch to the
    // viewport, so a bare-text body (no element children) reports no extent
    // past the viewport — its ink lives only in the Text paint items. Folding
    // their wrap-model extent in fixes both readers of content_h: the scroll
    // clamp (blitz#444 fixed overflowing elements but text-only bodies still
    // couldn't scroll past the first screenful) and the print pump's page
    // count (which saw viewport-clamped scrollHeight and cut blank tail
    // pages).
    let (ink_w, ink_h) = crate::diting_layout::paint::text_ink_extent(items);
    content_w = content_w.max(ink_w);
    content_h = content_h.max(ink_h);
    // Viewport overflow propagation (blitz#880, css-overflow-3 §3.3):
    // hidden/clip carried by the root element — its own or handed up by the
    // first body child — makes the viewport itself unscrollable, so the
    // scrolling area collapses to exactly the viewport, overriding the
    // unions above. Same clamp the `scroll_extent` op serves the JS side:
    // the two walks must agree or window.scrollY and the pump's clamp
    // disagree on the scroll range.
    if let Some(root) = root.filter(|r| rects.contains_key(r)) {
        let eff = crate::diting_layout::effective_viewport_overflow(
            dom,
            styles,
            root,
            |id| rects.contains_key(&id),
        );
        if matches!(
            eff,
            crate::diting_css::Overflow::Hidden | crate::diting_css::Overflow::Clip
        ) {
            content_w = vw;
            content_h = vh;
        }
    }
    let dx = (if scroll_x.is_finite() { scroll_x.max(0.0) } else { 0.0 })
        .min((content_w - vw).max(0.0));
    let dy = (if scroll_y.is_finite() { scroll_y.max(0.0) } else { 0.0 })
        .min((content_h - vh).max(0.0));

    // Sticky v2 paint half: resolve each layout-recorded span to the
    // total its items must carry — sticky spans take the node's read
    // total (box and all), scroller spans subtract the scroller's own
    // offset (its box and clip stay fixed while its content translates).
    // Sticky spans first so a both-node's sticky span precedes its
    // scroller span in the nesting order. Nothing shifted on the page →
    // paint the cached run untouched, zero copy. Per-frame translate, not
    // memoized: paint-only writes re-collect items without bumping the
    // tree epoch, so a cached shifted copy could go stale mid-animation.
    let sticky_map = sticky_shifts(gs, dom);
    let mut live: Vec<(usize, usize, [f32; 2])> = sticky_spans
        .iter()
        .filter_map(|&(n, s, e)| sticky_map.get(&n).map(|&v| (s, e, v)))
        .collect();
    live.extend(scroller_spans.iter().map(|&(n, s, e)| {
        let [ox, oy] = dom.node_scroll(n);
        let base = sticky_map.get(&n).copied().unwrap_or([0.0, 0.0]);
        (s, e, [base[0] - ox, base[1] - oy])
    }));
    // No zero-VALUE filter here: liveness is per-delta inside
    // apply_sticky_to_items (a pinned sticky inside a scroller has total
    // zero yet a nonzero delta over the scroller's base).
    let shifted_items: Option<Vec<crate::diting_layout::PaintItem>> = if live.is_empty() {
        None
    } else {
        Some(crate::diting_layout::apply_sticky_to_items(items, &live))
    };
    let items: &[crate::diting_layout::PaintItem] =
        shifted_items.as_deref().unwrap_or(items.as_slice());

    // Images the page references but the byte table lacks. Sources come
    // back absolutized against the document URL (resolve_img_source's base
    // join), so the missing entries are directly fetchable and match the
    // table's absolute keys on the re-blit.
    let mut missing: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(imgs) = dom.query_selector_all("img") {
        let table = gs.image_bytes.borrow();
        for nid in imgs {
            if let Some(src) =
                crate::diting_layout::resolve_img_source(dom, nid, vw, Some(gs.url.as_str()))
            {
                if !table.contains_key(&src) && seen.insert(src.clone()) {
                    missing.push(src);
                }
            }
        }
    }

    let mut canvas =
        crate::diting_layout::paint::Canvas::new_filled(vw as usize, vh as usize, [255, 255, 255, 255]);
    let fonts = crate::diting_fonts::font_book();
    // Vector-text collect: the items the PDF layer will redraw as glyphs drop
    // out of the raster pass (painting both would double the antialiasing).
    // Their ops come back band-local — the writer needs no per-band offset.
    let mut text_ops: Vec<crate::diting_layout::paint::PdfOp> = Vec::new();
    if collect_text {
        let (ops, vectorized) = crate::diting_layout::paint::pdf_text_ops(items, &fonts);
        let background: Vec<crate::diting_layout::PaintItem> = items
            .iter()
            .enumerate()
            .filter(|(i, _)| !vectorized.contains(i))
            .map(|(_, it)| it.clone())
            .collect();
        crate::diting_layout::paint::execute_band(&background, &fonts, &mut canvas, dx, dy);
        let mut ops = ops;
        // Band-window filter: ops are still document-space here (translate
        // below). Drop glyph lines whose baselines fall outside the band
        // (with a font-size margin) so each PDF page carries only its own
        // text instead of the whole document shifted off-page.
        let band_top = dy;
        let band_bottom = dy + canvas.height as f32;
        ops.retain(|op| match op {
            crate::diting_layout::paint::PdfOp::Line(l) => l.glyphs.iter().any(|g| {
                g.y > band_top - l.font_size * 1.5 && g.y < band_bottom + l.font_size
            }),
            _ => true,
        });
        crate::diting_layout::paint::pdf_ops_translate(&mut ops, dx, dy);
        text_ops = ops;
    } else {
        crate::diting_layout::paint::execute_band(items, &fonts, &mut canvas, dx, dy);
    }
    drop(guard);
    Some((
        BandFrame {
            width: canvas.width as u32,
            height: canvas.height as u32,
            rgba: canvas.data,
            dx,
            dy,
            content_size: (content_w, content_h),
            text_ops,
        },
        missing,
    ))
}

/// The page's text-ink extent alone ((width, height) in CSS px), from the
/// same memoized layout run every band paint shares. For bare-text bodies —
/// no element boxes past the stretched html/body — this is the only true
/// extent; the print pump reads it as its content-height fallback when
/// scrollHeight sits at the viewport clamp.
#[cfg(feature = "screenshot")]
pub(crate) fn text_ink_extent(gs: &JsState) -> Option<(f32, f32)> {
    let dom = gs.dom.as_ref()?;
    let epoch = dom.epoch();
    ensure_layout_run(gs, dom, epoch);
    let guard = gs.layout_cache.borrow();
    let (_, (_, _, _, items, _, _, _)) = guard.as_ref().filter(|(e, _)| *e == epoch)?;
    Some(crate::diting_layout::paint::text_ink_extent(items))
}

/// Insert a fetched image body into [`JsState::image_bytes`] (FIFO eviction
/// at the entry cap) and drop `layout_cache`: placeholder boxes and real
/// intrinsic sizes can reflow differently, so memoized geometry from the
/// pre-fetch run must not survive the insert.
#[cfg(feature = "screenshot")]
pub(crate) fn store_image_bytes(gs: &mut JsState, url: String, bytes: Vec<u8>) {
    const IMAGE_TABLE_CAP: usize = 64;
    let mut map = gs.image_bytes.borrow_mut();
    if map.contains_key(&url) {
        return;
    }
    if map.len() >= IMAGE_TABLE_CAP {
        if let Some(oldest) = gs.image_order.borrow_mut().pop_front() {
            map.remove(&oldest);
        }
    }
    map.insert(url.clone(), std::sync::Arc::new(bytes));
    drop(map);
    gs.image_order.borrow_mut().push_back(url);
    gs.drop_layout();
}

/// The property table the `computed_style` snapshot serializes — every
/// property [`computed_style_value`] can spell. Order is irrelevant (the
/// consumer is a JSON object); this list just guarantees the op and the
/// serializer stay in lockstep.
#[cfg(feature = "screenshot")]
const COMPUTED_STYLE_PROPS: &[&str] = &[
    "position",
    "z-index",
    "display",
    "float",
    "clear",
    "content",
    "box-sizing",
    "background-clip",
    "overflow",
    "overflow-x",
    "overflow-y",
    "border-collapse",
    "table-layout",
    "vertical-align",
    "text-decoration",
    "text-decoration-line",
    "font-size",
    "font-weight",
    "text-align",
    "line-height",
    "word-spacing",
    "font-variant-caps",
    "font-variant",
    "container-type",
    "container-name",
    "white-space",
    "text-overflow",
    "color",
    "background-color",
    "box-shadow",
    "backdrop-filter",
    "text-shadow",
    "background-image",
    "font-family",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "border-radius",
    "flex-direction",
    "flex-wrap",
    "justify-content",
    "align-items",
    "opacity",
    "transform",
    "animation-name",
    "animation-duration",
    "animation-delay",
    "animation-timing-function",
    "animation-fill-mode",
    "animation-iteration-count",
    "animation-direction",
    "animation-play-state",
    "transition-property",
    "transition-duration",
    "transition-delay",
    "transition-timing-function",
];

/// Chrome's UA-sheet display for the tags whose CSSOM value differs from the
/// Block bucket the layout engine gives them. Layout intentionally has no
/// table/list box types, so `td` lays out as a block — but getComputedStyle
/// must still answer like a browser (obscura #771: a table's box type was
/// unreadable over CDP). Only consulted while `display_from_ua` is set.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
fn ua_display_cssom(tag: Option<&str>) -> Option<&'static str> {
    match tag? {
        "table" => Some("table"),
        "tr" => Some("table-row"),
        "td" | "th" => Some("table-cell"),
        "thead" | "tbody" | "tfoot" => Some("table-row-group"),
        "col" => Some("table-column"),
        "colgroup" => Some("table-column-group"),
        "caption" => Some("table-caption"),
        "li" => Some("list-item"),
        _ => None,
    }
}

/// Chrome's computed spelling for a timing function (shared by the
/// animation-timing-function and transition-timing-function arms).
#[cfg(feature = "screenshot")]
fn easing_cssom(e: crate::diting_css::Easing) -> String {
    use crate::diting_css::Easing;
    match e {
        Easing::Linear => "linear".into(),
        Easing::CubicBezier(0.25, 0.1, 0.25, 1.0) => "ease".into(),
        Easing::CubicBezier(0.42, 0.0, 1.0, 1.0) => "ease-in".into(),
        Easing::CubicBezier(0.0, 0.0, 0.58, 1.0) => "ease-out".into(),
        Easing::CubicBezier(0.42, 0.0, 0.58, 1.0) => "ease-in-out".into(),
        Easing::CubicBezier(a, b, c, d) => format!(
            "cubic-bezier({}, {}, {}, {})",
            format_number(a),
            format_number(b),
            format_number(c),
            format_number(d)
        ),
    }
}

/// `", lower-roman"`-style suffix for counter()/counters() re-serialization;
/// decimal is the initial value so Chrome omits it.
#[cfg(feature = "screenshot")]
fn counter_style_suffix(style: crate::diting_css::CounterStyle) -> String {
    use crate::diting_css::CounterStyle;
    match style {
        CounterStyle::Decimal => String::new(),
        CounterStyle::DecimalLeadingZero => ", decimal-leading-zero".into(),
        CounterStyle::LowerAlpha => ", lower-alpha".into(),
        CounterStyle::UpperAlpha => ", upper-alpha".into(),
        CounterStyle::LowerRoman => ", lower-roman".into(),
        CounterStyle::UpperRoman => ", upper-roman".into(),
    }
}

/// Serialize one property of a cascaded [`ComputedStyle`] in Chrome's
/// computed-value spelling, for the `computed_style` op (getComputedStyle's
/// cascade layer). `None` means "not in the table" — the JS caller falls
/// through to its inline/dimension/default chain, so initial values never
/// shadow a caller that knows better.
#[cfg(feature = "screenshot")]
fn computed_style_value(
    s: &crate::diting_css::ComputedStyle,
    prop: &str,
    cssom_tag: Option<&str>,
) -> Option<String> {
    use crate::diting_css::*;
    let color = |c: &Color| {
        if c.3 == 255 {
            format!("rgb({}, {}, {})", c.0, c.1, c.2)
        } else {
            let a = format!("{:.3}", c.3 as f32 / 255.0);
            let a = a.trim_end_matches('0').trim_end_matches('.');
            format!("rgba({}, {}, {}, {})", c.0, c.1, c.2, if a.is_empty() { "0" } else { a })
        }
    };
    match prop {
        "position" => Some(
            match s.position {
                Some(PositionMode::Relative) => "relative",
                Some(PositionMode::Absolute) => "absolute",
                Some(PositionMode::Fixed) => "fixed",
                Some(PositionMode::Sticky) => "sticky",
                _ => "static",
            }
            .into(),
        ),
        "display" => {
            if s.display_from_ua {
                if let Some(ua) = ua_display_cssom(cssom_tag) {
                    return Some(ua.to_string());
                }
            }
            Some(
                match s.display {
                    Some(Display::Inline) => "inline",
                    Some(Display::InlineBlock) => "inline-block",
                    Some(Display::Flex) => "flex",
                    Some(Display::Grid) => "grid",
                    Some(Display::Table) => "table",
                    Some(Display::TableRow) => "table-row",
                    Some(Display::TableCell) => "table-cell",
                    Some(Display::None) => "none",
                    _ => "block",
                }
                .into(),
            )
        },
        "z-index" => Some(match s.z_index {
            Some(z) => z.to_string(),
            None => "auto".into(),
        }),
        "border-collapse" => Some(
            match s.border_collapse {
                Some(BorderCollapse::Collapse) => "collapse",
                _ => "separate",
            }
            .into(),
        ),
        "table-layout" => Some(
            match s.table_layout {
                Some(TableLayout::Fixed) => "fixed",
                _ => "auto",
            }
            .into(),
        ),
        // Declared values only: undeclared cells keep the JS caller's own
        // default chain (the UA middle behavior at the alignment site isn't
        // a declaration). Chrome's computed value: lengths are absolute px,
        // percentages keep their % shape.
        "vertical-align" => s
            .vertical_align
            .map(|va| match va {
                VerticalAlign::Top => "top".to_string(),
                VerticalAlign::Middle => "middle".to_string(),
                VerticalAlign::Bottom => "bottom".to_string(),
                VerticalAlign::Baseline => "baseline".to_string(),
                VerticalAlign::Sub => "sub".to_string(),
                VerticalAlign::Super => "super".to_string(),
                VerticalAlign::Length(px) => format!("{px}px"),
                VerticalAlign::Percent(p) => format!("{p}%"),
            }),
        // Not inherited (the paint-time propagation to inline descendants is
        // a layout concern): an element reports its OWN declared/UA set.
        "text-decoration-line" | "text-decoration" => Some(match s.text_decoration_line {
            Some(d) if !d.is_empty() => [
                if d.underline { "underline" } else { "" },
                if d.overline { "overline" } else { "" },
                if d.line_through { "line-through" } else { "" },
            ]
            .into_iter()
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
            _ => "none".to_string(),
        }),
        "float" => Some(
            match s.float_side {
                Some(FloatSide::Left) => "left",
                Some(FloatSide::Right) => "right",
                None => "none",
            }
            .into(),
        ),
        "clear" => Some(
            match s.clear_side {
                Some(ClearSide::Left) => "left",
                Some(ClearSide::Right) => "right",
                Some(ClearSide::Both) => "both",
                Some(ClearSide::InlineStart) => "inline-start",
                Some(ClearSide::InlineEnd) => "inline-end",
                None => "none",
            }
            .into(),
        ),
        // Chrome's computed value is the declared keyword itself (not a used
        // value), so the CSS initial surfaces as content-box.
        // Chrome's initial computed content is "normal"; declared strings
        // re-serialize quoted, attr()/counter()/counters()/quotes keywords
        // keep their functional shape. On pseudos the resolved plain string
        // surfaces instead (documented divergence).
        "content" => Some(match &s.content {
            Some(ContentValue::Str(t)) => {
                format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
            },
            Some(ContentValue::Attr(name)) => format!("attr({})", name),
            Some(ContentValue::Counter { name, style }) => {
                format!("counter({}{})", name, counter_style_suffix(*style))
            },
            Some(ContentValue::Counters { name, sep, style }) => format!(
                "counters({}, \"{}\"{})",
                name,
                sep.replace('\\', "\\\\").replace('"', "\\\""),
                counter_style_suffix(*style)
            ),
            Some(ContentValue::OpenQuote) => "open-quote".into(),
            Some(ContentValue::CloseQuote) => "close-quote".into(),
            Some(ContentValue::NoQuote { close }) => {
                if *close { "no-close-quote".into() } else { "no-open-quote".into() }
            },
            Some(ContentValue::List(parts)) => parts
                .iter()
                .map(|p| match p {
                    ContentValue::Str(t) => {
                        format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
                    },
                    ContentValue::Attr(name) => format!("attr({})", name),
                    ContentValue::Counter { name, style } => {
                        format!("counter({}{})", name, counter_style_suffix(*style))
                    },
                    ContentValue::Counters { name, sep, style } => format!(
                        "counters({}, \"{}\"{})",
                        name,
                        sep.replace('\\', "\\\\").replace('"', "\\\""),
                        counter_style_suffix(*style)
                    ),
                    ContentValue::OpenQuote => "open-quote".into(),
                    ContentValue::CloseQuote => "close-quote".into(),
                    ContentValue::NoQuote { close } => {
                        if *close { "no-close-quote" } else { "no-open-quote" }.into()
                    },
                    ContentValue::List(_) => "normal".into(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            None => "normal".into(),
        }),
        "box-sizing" => Some(
            match s.box_sizing {
                Some(BoxSizing::BorderBox) => "border-box",
                _ => "content-box",
            }
            .into(),
        ),
        // Same declared-keyword convention: `text` or the CSS initial's
        // border-box. The -webkit- alias reads through here too.
        "background-clip" => Some(if s.background_clip_text { "text" } else { "border-box" }.into()),
        "overflow" => {
            let (x, y) = s.resolved_overflow();
            Some(if x == y {
                crate::diting_css::overflow_name(x).into()
            } else {
                format!("{} {}", crate::diting_css::overflow_name(x), crate::diting_css::overflow_name(y))
            })
        }
        "overflow-x" => {
            let (x, _) = s.resolved_overflow();
            Some(crate::diting_css::overflow_name(x).into())
        }
        "overflow-y" => {
            let (_, y) = s.resolved_overflow();
            Some(crate::diting_css::overflow_name(y).into())
        }
        "font-size" => s.font_size.map(|f| format!("{}px", f)),
        "font-weight" => s.font_weight.map(|w| w.to_string()),
        "text-align" => Some(
            match s.text_align {
                Some(TextAlign::Center) => "center",
                Some(TextAlign::Right) => "right",
                _ => "left",
            }
            .into(),
        ),
        "line-height" => Some(
            match s.line_height {
                Some(LineHeightSpec::Number(n)) => format_number(n),
                Some(LineHeightSpec::Px(v)) => format!("{}px", format_number(v)),
                _ => "normal".into(),
            },
        ),
        "word-spacing" => Some(
            match s.word_spacing {
                Some(v) => format!("{}px", format_number(v)),
                None => "normal".into(),
            },
        ),
        "font-variant-caps" => Some(
            match s.font_variant_caps {
                Some(true) => "small-caps".into(),
                _ => "normal".into(),
            },
        ),
        "font-variant" => Some(
            match s.font_variant_caps {
                Some(true) => "small-caps".into(),
                _ => "normal".into(),
            },
        ),
        "container-type" => Some(
            match s.container_type {
                crate::diting_css::ContainerType::InlineSize => "inline-size",
                crate::diting_css::ContainerType::Size => "size",
                _ => "normal",
            }
            .into(),
        ),
        "container-name" => Some(s.container_name.clone().unwrap_or_else(|| "none".into())),
        "white-space" => Some(
            match s.white_space {
                Some(crate::diting_css::WhiteSpace::Nowrap) => "nowrap",
                Some(crate::diting_css::WhiteSpace::Pre) => "pre",
                Some(crate::diting_css::WhiteSpace::PreWrap) => "pre-wrap",
                Some(crate::diting_css::WhiteSpace::PreLine) => "pre-line",
                Some(crate::diting_css::WhiteSpace::BreakSpaces) => "break-spaces",
                _ => "normal",
            }
            .into(),
        ),
        "text-overflow" => Some(
            match s.text_overflow {
                Some(crate::diting_css::TextOverflow::Ellipsis) => "ellipsis",
                _ => "clip",
            }
            .into(),
        ),
        "color" => s.color.as_ref().map(&color),
        "background-color" => s.background_color.as_ref().map(&color),
        // Raw author token stream (var() already substituted by the cascade);
        // unset stays absent so the JS initial (`none`) serves it.
        "background-image" => s.background_image.clone(),
        // Same posture as background-image: the author's stack, verbatim.
        "font-family" => s.font_family.clone(),
        // Padding/margin longhands: Chrome reports the used px. Percent and
        // calc keep their declared spelling (resolving needs the containing
        // block this layer has no geometry for), unset is 0 — all closer to
        // truth than the JS mask's blanket "0px", which swallowed real
        // padding from every stylesheet-declared element (the pptx-native
        // walker read all-zero insets off this).
        "padding-top" => Some(side_css(&s.padding.top)),
        "padding-right" => Some(side_css(&s.padding.right)),
        "padding-bottom" => Some(side_css(&s.padding.bottom)),
        "padding-left" => Some(side_css(&s.padding.left)),
        "margin-top" => Some(side_css(&s.margin.top)),
        "margin-right" => Some(side_css(&s.margin.right)),
        "margin-bottom" => Some(side_css(&s.margin.bottom)),
        "margin-left" => Some(side_css(&s.margin.left)),
        // Chrome's computed border-radius collapses equal corners (up to the
        // shortest form that round-trips); elliptical corners serialize with
        // the slash form. Percent stays percent — resolving against the box
        // is a used value this layer has no geometry for, and "12%" is closer
        // to truth than the JS mask's "0px".
        "border-radius" => Some({
            let spell = |l: &Length| match l {
                Length::Px(v) => format!("{}px", format_number(*v)),
                Length::Percent(p) => format!("{}%", format_number(*p)),
                // Keyword lengths never parse into a corner radius; the
                // fallback just keeps the arm total.
                _ => "0px".to_string(),
            };
            match &s.corner_radii {
                Some([(tl_x, tl_y), (tr_x, tr_y), (br_x, br_y), (bl_x, bl_y)]) => {
                    let x = format!(
                        "{} {} {} {}",
                        spell(tl_x),
                        spell(tr_x),
                        spell(br_x),
                        spell(bl_x)
                    );
                    let y = format!(
                        "{} {} {} {}",
                        spell(tl_y),
                        spell(tr_y),
                        spell(br_y),
                        spell(bl_y)
                    );
                    if x == y {
                        // Circular corners: collapse like Chrome (a b / a b /
                        // a b c d → a / a b / a b c d).
                        let parts: Vec<&str> = x.split(' ').collect();
                        let [a, b, c, d] = [parts[0], parts[1], parts[2], parts[3]];
                        let s = if c == a && d == b {
                            if a == b { a.to_string() } else { format!("{a} {b}") }
                        } else {
                            format!("{a} {b} {c} {d}")
                        };
                        s
                    } else {
                        format!("{x} / {y}")
                    }
                }
                None => "0px".to_string(),
            }
        }),
        // Chrome's computed box-shadow: one entry per layer (first-declared
        // first), each "inset? color dx dy blur spread"; unset serializes
        // as "none".
        "box-shadow" => Some(match &s.box_shadow {
            None => "none".to_string(),
            Some(layers) => layers
                .iter()
                .map(|l| {
                    let c = color(&l.color);
                    let inset = if l.inset { "inset " } else { "" };
                    format!(
                        "{inset}{} {}px {}px {}px {}px",
                        c,
                        format_number(l.dx),
                        format_number(l.dy),
                        format_number(l.blur),
                        format_number(l.spread)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        }),
        // Chrome's computed backdrop-filter: "blur(<length>)" or "none".
        "backdrop-filter" => Some(match s.backdrop_blur {
            None => "none".to_string(),
            Some(px) => format!("blur({}px)", format_number(px)),
        }),
        // Chrome's computed text-shadow: "color dx dy blur" per layer
        // (first-declared first); unset serializes as "none".
        "text-shadow" => Some(match &s.text_shadow {
            None => "none".to_string(),
            Some(layers) => layers
                .iter()
                .map(|l| {
                    format!(
                        "{} {}px {}px {}px",
                        color(&l.color),
                        format_number(l.dx),
                        format_number(l.dy),
                        format_number(l.blur)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        }),
        "flex-direction" => Some(
            match s.flex_direction {
                Some(FlexDirection::RowReverse) => "row-reverse",
                Some(FlexDirection::Column) => "column",
                Some(FlexDirection::ColumnReverse) => "column-reverse",
                _ => "row",
            }
            .into(),
        ),
        "flex-wrap" => Some(
            match s.flex_wrap {
                Some(FlexWrapMode::Wrap) => "wrap",
                _ => "nowrap",
            }
            .into(),
        ),
        "justify-content" => Some(
            match s.justify_content {
                Some(JustifyMode::FlexStart) => "flex-start",
                Some(JustifyMode::Center) => "center",
                Some(JustifyMode::FlexEnd) => "flex-end",
                Some(JustifyMode::SpaceBetween) => "space-between",
                Some(JustifyMode::SpaceAround) => "space-around",
                Some(JustifyMode::SpaceEvenly) => "space-evenly",
                // Unset computes to `normal` in Chrome; only an author
                // declaration yields a keyword (obscura #771 wrong-value rows).
                None => "normal",
            }
            .into(),
        ),
        "align-items" => Some(
            match s.align_items {
                Some(AlignMode::Stretch) => "stretch",
                Some(AlignMode::FlexStart) => "flex-start",
                Some(AlignMode::Center) => "center",
                Some(AlignMode::FlexEnd) => "flex-end",
                None => "normal",
            }
            .into(),
        ),
        // Animation batch A: opacity is non-inherited with initial 1; Chrome
        // spells the computed number bare ("1", "0.5781"). Undeclared still
        // answers here — "1" IS the initial value, so no caller chain can
        // know better.
        "opacity" => Some(format_number(s.opacity.unwrap_or(1.0))),
        // Animation batch B / affine batch: the Transform2D serializes as
        // its full CSS matrix (a b c d tx ty) — rotate/skew/matrix join the
        // same composition now. No transform (or `none`) computes to "none";
        // a percentage translate resolves against the element's own border
        // box — a used value this snapshot layer has no box for — so those
        // stay absent and the JS inline chain answers instead of a wrong
        // matrix (GSAP writes inline, so tween state always reflects).
        "transform" => match &s.transform {
            None => Some("none".into()),
            Some(t) => match (t.tx, t.ty) {
                (Length::Px(tx), Length::Px(ty)) => Some(format!(
                    "matrix({}, {}, {}, {}, {}, {})",
                    format_number(t.a),
                    format_number(t.b),
                    format_number(t.c),
                    format_number(t.d),
                    format_number(tx),
                    format_number(ty)
                )),
                _ => None,
            },
        },
        // CSS animation longhands resolved from the shorthand's AnimationSpec
        // (the engine stores only the shorthand — parse_animation_shorthand).
        // Chrome spells durations in bare seconds ("0.1s"); direction and
        // play-state are unmodeled in the sampler, so they report their
        // initials ("normal", "running") like every other unmodeled longhand.
        "animation-name" => Some(
            s.animation
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "none".into()),
        ),
        "animation-duration" => Some(format!(
            "{}s",
            format_number(s.animation.as_ref().map(|a| a.duration).unwrap_or(0.0))
        )),
        "animation-delay" => Some(format!(
            "{}s",
            format_number(s.animation.as_ref().map(|a| a.delay).unwrap_or(0.0))
        )),
        "animation-timing-function" => Some(easing_cssom(
            s.animation
                .as_ref()
                .map(|a| a.easing)
                .unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        )),
        "animation-fill-mode" => Some(
            match s.animation.as_ref().map(|a| a.fill_forwards) {
                Some(true) => "forwards",
                _ => "none",
            }
            .into(),
        ),
        "animation-iteration-count" => Some(match s.animation.as_ref().map(|a| a.iterations) {
            Some(v) if v.is_infinite() => "infinite".into(),
            Some(v) => format_number(v),
            None => "1".into(),
        }),
        "animation-direction" => Some("normal".into()),
        "animation-play-state" => Some("running".into()),
        // CSS transition longhands from the shorthand's TransitionSpec. Chrome
        // spells an unset transition-property "all" (the initial), and the
        // spec table's "none" keyword only appears when declared.
        "transition-property" => Some(
            s.transition
                .as_ref()
                .and_then(|t| t.property.clone())
                .unwrap_or_else(|| "all".into()),
        ),
        "transition-duration" => Some(format!(
            "{}s",
            format_number(s.transition.as_ref().map(|t| t.duration).unwrap_or(0.0))
        )),
        "transition-delay" => Some(format!(
            "{}s",
            format_number(s.transition.as_ref().map(|t| t.delay).unwrap_or(0.0))
        )),
        "transition-timing-function" => Some(easing_cssom(
            s.transition
                .as_ref()
                .map(|t| t.easing)
                .unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        )),
        _ => None,
    }
}

/// Minimal float spelling: integral values print without a fraction (Chrome
/// reports `24px`, not `24.00px`).
#[cfg(feature = "screenshot")]
fn format_number(v: f32) -> String {
    if (v - v.round()).abs() < f32::EPSILON {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

/// One side of a padding/margin shorthand in Chrome's computed spelling.
/// Unset sides are `0px` (the CSS initial for both properties).
#[cfg(feature = "screenshot")]
fn side_css(l: &Option<crate::diting_css::Length>) -> String {
    use crate::diting_css::Length;
    match l {
        None => "0px".to_string(),
        Some(Length::Px(v)) => format!("{}px", format_number(*v)),
        Some(Length::Percent(p)) => format!("{}%", format_number(*p)),
        Some(Length::Calc { percent, px }) => {
            format!("calc({}% + {}px)", format_number(*percent), format_number(*px))
        }
        // `auto` is margin-only (the centering idiom); padding rejects it
        // at parse time so it can't reach this arm from there. min/max/
        // fit-content are width keywords and likewise never reach a
        // padding/margin side — the fallback just keeps the arm total.
        Some(Length::Auto) => "auto".to_string(),
        Some(Length::MinContent | Length::MaxContent | Length::FitContent) => "0px".to_string(),
    }
}

/// Index of `n` among its parent's children (0-based).
fn node_child_index(dom: &DomTree, n: NodeId) -> usize {
    let mut i = 0usize;
    let mut cur = dom.get_node(n).and_then(|x| x.prev_sibling);
    while let Some(p) = cur {
        i += 1;
        cur = dom.get_node(p).and_then(|x| x.prev_sibling);
    }
    i
}

/// Ancestor chain of `n` from the root down to `n` (root first).
fn node_ancestors_root_first(dom: &DomTree, n: NodeId) -> Vec<NodeId> {
    let mut v = vec![n];
    let mut cur = n;
    while let Some(p) = dom.get_node(cur).and_then(|x| x.parent) {
        v.push(p);
        cur = p;
    }
    v.reverse();
    v
}

/// Preorder (document) order comparison of two nodes: -1 before, 1 after, 0 same.
fn compare_node_order(dom: &DomTree, a: NodeId, b: NodeId) -> i32 {
    if a == b {
        return 0;
    }
    let aa = node_ancestors_root_first(dom, a);
    let bb = node_ancestors_root_first(dom, b);
    // Different roots: order is undefined per spec; keep it stable by node id.
    if aa[0] != bb[0] {
        return if a.index() < b.index() { -1 } else { 1 };
    }
    let mut i = 0usize;
    while i < aa.len() && i < bb.len() && aa[i] == bb[i] {
        i += 1;
    }
    if i >= aa.len() {
        return -1; // a is an ancestor of b -> a precedes
    }
    if i >= bb.len() {
        return 1; // b is an ancestor of a -> a follows
    }
    if node_child_index(dom, aa[i]) < node_child_index(dom, bb[i]) {
        -1
    } else {
        1
    }
}

#[op2(fast)]
fn op_console_msg(state: &OpState, #[string] level: &str, #[string] msg: &str) {
    match level {
        "warn" => tracing::warn!(target: "diting::console", "{}", msg),
        "error" => tracing::error!(target: "diting::console", "{}", msg),
        _ => tracing::info!(target: "diting::console", "{}", msg),
    }
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    // Queue-time URL: the consumer may drain long after a navigation moved
    // the page elsewhere, and per-entry URLs make session_console's
    // url_contains filter truthful.
    let log_url = gs.url.clone();
    gs.pending_console_calls
        .push((level.to_string(), msg.to_string(), log_url));
}

/// window.alert/confirm/prompt land here from the bootstrap stubs. There is
/// no UI to attach a dialog to, so blocking is not an option (the thread
/// that would show the dialog is the same one running the page script);
/// instead each call is answered from the session-side policy —
/// `dialog_accept` (default false = dismiss) plus `dialog_prompt_text` for
/// prompt — and recorded as a level-"dialog" console entry so
/// session_console shows what the page asked. Returns
/// `{"accept":bool,"value":string|null}`; value is only meaningful for
/// prompt, where the JS wrapper falls back to the call's default argument.
#[op2]
#[string]
fn op_dialog(state: &OpState, #[string] kind: &str, #[string] message: &str) -> String {
    let shared = state.borrow::<SharedState>().clone();
    let mut gs = shared.borrow_mut();
    let (accept, value) = match kind {
        "confirm" => (gs.dialog_accept, None),
        "prompt" => (gs.dialog_accept, gs.dialog_prompt_text.clone()),
        // alert: nothing to answer, recorded for observability only.
        _ => (true, None),
    };
    let dialog_url = gs.url.clone();
    let mut payload = serde_json::json!({ "dialog": kind, "message": message });
    if kind == "confirm" || kind == "prompt" {
        payload["answer"] = serde_json::Value::Bool(accept);
        if kind == "prompt" {
            payload["value"] = value.clone().into();
        }
    }
    gs.pending_console_calls
        .push(("dialog".to_string(), payload.to_string(), dialog_url));
    serde_json::json!({ "accept": accept, "value": value }).to_string()
}

// op_fetch_url backs JS-level `fetch()` and XHR. Pre-#139 it used a
// process-wide `OnceLock<reqwest::Client>` initialised with no proxy, so
// every JS network call bypassed the configured upstream proxy. We now
// build a client per request, threading whatever `proxy_url` the page's
// HttpClient was configured with.
//
// The per-request build cost is negligible (≪1ms) compared with the actual
// network round-trip; the simplification is worth not having to invalidate
// a cache when the proxy is reconfigured between fetches.
//
// Process-wide cache keyed by proxy URL. Previously we built a fresh
// reqwest::Client on every op_fetch_url call (every JS fetch(), XHR,
// dynamic script load). Each build re-initialised TLS roots and a
// fresh connection pool with zero reuse, costing ~5ms per fetch on top
// of any real network work. On an asset-heavy page with 30+ subresources
// that adds ~150ms of pure waste. With the cache, the first fetch on a
// given proxy pays the build cost once and every subsequent fetch reuses
// the same connection pool.
static FETCH_CLIENT_CACHE: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<String, reqwest::Client>>,
> = std::sync::OnceLock::new();

/// Shared HTTP client cache for any code in diting-js that needs a
/// reqwest::Client (op_fetch_url for JS-side fetch/XHR, the ES module
/// loader for dynamic imports). Keyed by proxy URL ("" = direct).
/// One client per distinct proxy, reused for every request, so the
/// connection pool actually warms up.
pub fn cached_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    let key = proxy_url.unwrap_or("").to_string();
    let cache = FETCH_CLIENT_CACHE
        .get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Ok(read) = cache.read() {
        if let Some(client) = read.get(&key) {
            return Ok(client.clone());
        }
    }
    let client = build_request_client(proxy_url)?;
    if let Ok(mut write) = cache.write() {
        write.entry(key).or_insert_with(|| client.clone());
    }
    Ok(client)
}

/// Pick a reqwest client for `url`. When the page was opened without a proxy
/// (`proxy_url` is None), subresources also go direct. When the page opened
/// through a proxy (foreign sites), subresources follow it too.
async fn select_request_client(_url: &str, proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    cached_request_client(proxy_url)
}

fn build_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    // Redirects are followed manually below so each hop can be re-validated
    // against the same SSRF policy as the initial URL (GHSA-8v6v-g4rh-jmcm).
    // With reqwest's default auto-follow, an attacker-controlled origin can
    // 302 to http://127.0.0.1 and read the internal-service body.
    // Per-request timeout so a scripted fetch()/XHR, or a CORS preflight OPTIONS
    // (issue #251), to a server that accepts the connection but never responds
    // cannot hang forever. Without it op_fetch_url never returns, the fetch
    // promise never settles, and the JS XHR is stuck at readyState 1 with no
    // completion event (which stranded Angular HttpClient). On timeout reqwest's
    // send().await errors, which op_fetch_url propagates and the fetch shim turns
    // into an XHR `error`/`loadend`. 30s matches the other clients in the
    // workspace; AGINXBROWSER_FETCH_TIMEOUT_MS overrides it for tighter cloud limits.
    let timeout_ms: u64 = std::env::var("AGINXBROWSER_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let mut builder = crate::diting_net::client::reqwest_builder_no_env_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .connect_timeout(std::time::Duration::from_secs(10))
        // Be explicit about pool hygiene: these clients are cached
        // process-wide (FETCH_CLIENT_CACHE), and a half-dead idle connection
        // handed back out of the pool stalls every later request to that
        // origin (the "long-run dispatch degradation", bug #24). A short
        // idle window plus TCP keepalive reaps stale connections instead of
        // trusting them.
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .tcp_keepalive(std::time::Duration::from_secs(30));
    if let Some(proxy) = proxy_url {
        let p = reqwest::Proxy::all(proxy)
            .map_err(|e| format!("Invalid op_fetch_url proxy '{}': {}", proxy, e))?;
        builder = builder.proxy(p);
    }
    builder
        .build()
        .map_err(|e| format!("failed to build reqwest::Client: {}", e))
}

/// Cap on the number of redirect hops op_fetch_url will follow.
///
/// The Fetch standard fixes the number at 20: HTTP-redirect fetch returns a
/// network error as soon as a request's redirect count *reaches* 20, so the
/// twentieth hop still succeeds and the twenty-first fails.
/// https://fetch.spec.whatwg.org/#http-redirect-fetch
///
/// The reqwest default of 10 does not apply here: redirects are followed by
/// hand in this file, one hop per loop iteration, so each hop is re-checked
/// against the SSRF rules (upstream 4b90ec3).
const FETCH_REDIRECT_LIMIT: usize = 20;

/// RequestCredentials from the Fetch standard: whether cookies may be sent to
/// (and stored from) a request's URL (upstream b744b9b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchCredentials {
    Omit,
    SameOrigin,
    Include,
}

impl FetchCredentials {
    fn parse(value: &str) -> Self {
        match value {
            "omit" => Self::Omit,
            "include" => Self::Include,
            _ => Self::SameOrigin,
        }
    }

    fn allows(self, page_origin: &str, request_url: &str) -> bool {
        match self {
            Self::Omit => false,
            Self::Include => true,
            Self::SameOrigin => request_origin(request_url)
                .map(|origin| origin == page_origin)
                .unwrap_or(false),
        }
    }
}

fn request_origin(request_url: &str) -> Option<String> {
    url::Url::parse(request_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// A CORS response (or preflight) must match the credentials mode:
/// credentialed requests require the exact origin plus
/// Access-Control-Allow-Credentials: true.
fn cors_response_allows(
    credentials: FetchCredentials,
    page_origin: &str,
    allowed_origin: &str,
    allow_credentials: &str,
) -> bool {
    if credentials == FetchCredentials::Include {
        allowed_origin == page_origin && allow_credentials == "true"
    } else {
        allowed_origin == "*" || allowed_origin == page_origin
    }
}

fn is_cors_safelisted_method(method: &reqwest::Method) -> bool {
    matches!(method.as_str(), "GET" | "HEAD" | "POST")
}

fn is_cors_unsafe_request_header_byte(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t')
        || matches!(
            byte,
            b'"' | b'(' | b')' | b':' | b'<' | b'>' | b'?' | b'@' | b'[' | b'\\'
                | b']' | b'{' | b'}' | 0x7f
        )
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

fn is_cors_safelisted_content_type(value: &str) -> bool {
    if value.bytes().any(is_cors_unsafe_request_header_byte) {
        return false;
    }

    // A MIME type must have a valid type/subtype before its parameters. This
    // is deliberately narrower than merely splitting at ';': malformed values
    // must not turn an application/json request into a simple request.
    let essence = value
        .split_once(';')
        .map_or(value, |(essence, _)| essence)
        .trim_matches([' ', '\t']);
    let Some((type_, subtype)) = essence.split_once('/') else {
        return false;
    };
    if type_.is_empty()
        || subtype.is_empty()
        || !type_.bytes().all(is_http_token_byte)
        || !subtype.bytes().all(is_http_token_byte)
    {
        return false;
    }

    essence.eq_ignore_ascii_case("application/x-www-form-urlencoded")
        || essence.eq_ignore_ascii_case("multipart/form-data")
        || essence.eq_ignore_ascii_case("text/plain")
}

fn decimal_is_at_most(left: &str, right: &str) -> bool {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len() < right.len() || (left.len() == right.len() && left <= right)
}

fn is_cors_safelisted_range(value: &str) -> bool {
    let Some(range) = value.strip_prefix("bytes=") else {
        return false;
    };
    let Some((start, end)) = range.split_once('-') else {
        return false;
    };
    if start.is_empty()
        || !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    end.is_empty() || decimal_is_at_most(start, end)
}

fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    if name.eq_ignore_ascii_case("accept") {
        return !value.bytes().any(is_cors_unsafe_request_header_byte);
    }
    if name.eq_ignore_ascii_case("accept-language")
        || name.eq_ignore_ascii_case("content-language")
    {
        return value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
        });
    }
    if name.eq_ignore_ascii_case("content-type") {
        return is_cors_safelisted_content_type(value);
    }
    if name.eq_ignore_ascii_case("range") {
        return is_cors_safelisted_range(value);
    }
    false
}

/// Return the sorted, lowercase header names that must be authorized by a
/// CORS preflight. The aggregate safelist cap is observable only on unusual
/// requests and does not add work to same-origin requests.
fn cors_unsafe_request_header_names(headers: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut safelist_value_size = 0usize;

    for (name, value) in headers {
        if is_cors_safelisted_request_header(name, value) {
            safelist_value_size = safelist_value_size.saturating_add(value.len());
        } else {
            unsafe_names.push(name.to_ascii_lowercase());
        }
    }
    if safelist_value_size > 1024 {
        unsafe_names.extend(
            headers
                .iter()
                .filter(|(name, value)| is_cors_safelisted_request_header(name, value))
                .map(|(name, _)| name.to_ascii_lowercase()),
        );
    }
    unsafe_names.sort_unstable();
    unsafe_names.dedup();
    unsafe_names
}

fn parse_cors_header_list<'a>(
    headers: &'a reqwest::header::HeaderMap,
    name: &'static str,
) -> Option<Vec<&'a str>> {
    let mut items = Vec::new();
    for value in headers.get_all(name).iter() {
        let value = value.to_str().ok()?;
        for item in value.split(',') {
            let item = item.trim_matches([' ', '\t']);
            if item.is_empty() || !item.bytes().all(is_http_token_byte) {
                return None;
            }
            items.push(item);
        }
    }
    Some(items)
}

fn preflight_allows_method(method: &reqwest::Method, allowed: &[&str], credentialed: bool) -> bool {
    is_cors_safelisted_method(method)
        || allowed.iter().any(|allowed| {
            *allowed == method.as_str() || (*allowed == "*" && !credentialed)
        })
}

fn preflight_allows_header(name: &str, allowed: &[&str], credentialed: bool) -> bool {
    allowed
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
        || (!name.eq_ignore_ascii_case("authorization")
            && !credentialed
            && allowed.contains(&"*"))
}

/// op_fetch_url's terminal response. The manual redirect walk yields a live
/// reqwest response; the legacy-TLS fallback (transport failure on the raw
/// client) yields an already-buffered engine response whose redirects were
/// resolved — and per-hop revalidated — inside the HttpClient.
enum OpFetchOutcome {
    Live(reqwest::Response),
    Buffered(crate::diting_net::Response),
}

/// Network-event payload the walk hands back to its driver, which records it
/// into OpState (`fetch-{N}` id space, body store, js_network_events) once
/// the walk returns and OpState is reachable again.
struct FetchNetworkEvent {
    url: String,
    method: String,
    status: u16,
    response_headers: std::collections::HashMap<String, String>,
    body_size: usize,
    stored_text: Option<String>,
    resp_body_base64: String,
}

/// What [`fetch_url_walk`] produces: the exact JSON envelope the op returns,
/// plus the network event to record (None on the preflight-reject paths,
/// which carry their failure through `deps.failures` instead).
struct FetchWalkOutcome {
    json: String,
    network: Option<FetchNetworkEvent>,
}

/// Send-clonable inputs both fetch drivers (the deferred async op and the
/// sync-XHR op) hand to [`fetch_url_walk`]. Nothing here borrows OpState —
/// the sync driver runs the walk on a worker thread, where the Rc/RefCell
/// state cannot follow. Failure network events recorded mid-walk are
/// collected here and replayed by the driver.
struct FetchWalkDeps {
    url: String,
    method: String,
    custom_headers: std::collections::HashMap<String, String>,
    body_bytes: Vec<u8>,
    page_origin: String,
    mode: String,
    credentials: FetchCredentials,
    cookie_jar: Option<Arc<CookieJar>>,
    in_flight: Option<Arc<std::sync::atomic::AtomicU32>>,
    http_client: Option<Arc<HttpClient>>,
    proxy_url: Option<String>,
    document_url: String,
    referrer_policy: String,
    referrer_init: String,
    callbacks: Option<std::sync::Arc<crate::diting_net::CallbackRegistry>>,
    failures: Vec<(String, String, String)>,
}

/// OpState-cloned inputs the fetch front half gathers before any network
/// I/O, shared by both drivers.
struct FetchRequestInit {
    cookie_jar: Option<Arc<CookieJar>>,
    in_flight: Option<Arc<std::sync::atomic::AtomicU32>>,
    proxy_url: Option<String>,
    http_client: Option<Arc<HttpClient>>,
    callbacks: Option<std::sync::Arc<crate::diting_net::CallbackRegistry>>,
    document_url: String,
    intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
}

/// Shared front half of op_fetch_url and op_fetch_url_sync: the SSRF gate,
/// the base64-body marker probe, and the OpState clones both drivers hand
/// to the walk. Returns Err(the exact JSON the op must return) for the two
/// short-circuits (SSRF reject, setBlockedURLs pattern), each recorded into
/// the network-event log first.
fn gather_fetch_parts(
    state: &OpState,
    url: &str,
    method: &str,
    headers_json: &str,
) -> Result<(FetchRequestInit, bool), String> {
    if let Ok(parsed_url) = url::Url::parse(url) {
        if let Err(e) = validate_fetch_url(&parsed_url) {
            record_failed_fetch(state, url, method, e.clone());
            return Err(serde_json::json!({
                "status": 0,
                "body": "",
                "url": url,
                "headers": {},
                "blocked": true,
                "error": e,
            }).to_string());
        }
    }

    // The JS shim sets this out-of-band header to signal that `body` is
    // base64-encoded raw bytes (upstream obscura #716). Detected from the
    // original headers_json, before any interception rewrite, since a
    // Continue rewrite supplies a plain-text body, never the base64 wire form.
    let body_is_base64 = serde_json::from_str::<serde_json::Value>(headers_json)
        .ok()
        .and_then(|v| {
            v.get("__diting_body_b64")
                .and_then(|x| x.as_str())
                .map(|s| s == "1")
        })
        .unwrap_or(false);

    let (init, blocked_pattern) = {
        let gs = state.borrow::<SharedState>().clone();
        let gs = gs.borrow_mut();
        let blocked_pattern = gs
            .blocked_urls
            .iter()
            .find(|p| *p == "*" || url.contains(p.as_str()) || glob_match(p, url))
            .cloned();
        tracing::debug!("op_fetch_url: intercept_enabled={}, has_tx={}", gs.intercept_enabled, gs.intercept_tx.is_some());
        let init = FetchRequestInit {
            cookie_jar: gs.cookie_jar.clone(),
            in_flight: gs.http_client.as_ref().map(|c| c.in_flight.clone()),
            // #139: thread the configured proxy through to the per-request
            // reqwest::Client. Without this, op_fetch_url silently bypasses
            // BrowserContext.proxy_url for every JS fetch() / XHR call.
            proxy_url: gs.http_client.as_ref().and_then(|c| c.proxy_url().map(|s| s.to_string())),
            http_client: gs.http_client.clone(),
            callbacks: gs.callbacks.clone(),
            document_url: gs.url.clone(),
            intercept_tx: if gs.intercept_enabled {
                gs.intercept_tx.clone()
            } else {
                None
            },
        };
        (init, blocked_pattern)
    };
    // Recorded after the scope closes — record_failed_fetch re-borrows
    // this state, which cannot nest under the held borrow_mut.
    if let Some(pattern) = blocked_pattern {
        record_failed_fetch(
            state,
            url,
            method,
            format!("blocked by Network.setBlockedURLs pattern: {pattern}"),
        );
        return Err(serde_json::json!({
            "status": 0,
            "body": "",
            "url": url,
            "headers": {},
            "blocked": true,
        }).to_string());
    }
    Ok((init, body_is_base64))
}

/// Records a successful fetch's body + network event (upstream #406/#360):
/// keyed `fetch-{N}`, LRU-bounded, so the CDP layer can emit Network events
/// and resolve getResponseBody for fetch()/XHR traffic. Extracted from the
/// walk so the sync driver can record after its worker thread returns.
fn record_fetch_network_event(state: &OpState, ev: &FetchNetworkEvent) -> String {
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    gs.network_response_body_counter += 1;
    let request_id = format!("fetch-{}", gs.network_response_body_counter);
    let max_entries = response_body_entry_limit();
    let max_bytes = response_body_byte_limit();
    let (stored_body, base64_encoded) = match &ev.stored_text {
        Some(text) => (text.clone(), false),
        None => (ev.resp_body_base64.clone(), true),
    };
    if max_entries > 0 && max_bytes > 0 && ev.body_size <= max_bytes {
        gs.network_response_bodies.insert(
            request_id.clone(),
            StoredNetworkResponseBody {
                body: stored_body,
                base64_encoded,
            },
        );
        gs.network_response_body_order.push_back(request_id.clone());
        while gs.network_response_body_order.len() > max_entries {
            if let Some(oldest) = gs.network_response_body_order.pop_front() {
                gs.network_response_bodies.remove(&oldest);
            }
        }
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    gs.js_network_events.push(JsNetworkEvent {
        request_id: request_id.clone(),
        url: ev.url.clone(),
        method: ev.method.clone(),
        status: ev.status,
        response_headers: ev.response_headers.clone(),
        body_size: ev.body_size,
        timestamp,
        error: None,
    });
    const MAX_JS_NETWORK_EVENTS: usize = 4096;
    if gs.js_network_events.len() > MAX_JS_NETWORK_EVENTS {
        let overflow = gs.js_network_events.len() - MAX_JS_NETWORK_EVENTS;
        gs.js_network_events.drain(0..overflow);
    }
    request_id
}

/// Replays the failure events the walk collected into the network-event log
/// (a sequential walk yields at most one).
fn replay_fetch_failures(state: &OpState, failures: &[(String, String, String)]) {
    for (url, method, reason) in failures {
        record_failed_fetch(state, url, method, reason.clone());
    }
}

#[op2(async(deferred), fast)]
#[string]
async fn op_fetch_url(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    #[string] method: String,
    #[string] headers_json: String,
    #[string] body: String,
    #[string] origin: String,
    #[string] mode: String,
    #[string] credentials: String,
    // "policy\0init" — bundled to stay under deno_core's async op arg limit.
    #[string] referrer: String,
) -> Result<String, deno_error::JsErrorBox> {
    // deno_core async op codegen caps explicit args; the Referer pair rides one slot.
    let (referrer_policy, referrer_init) = match referrer.split_once('\u{0}') {
        Some((p, r)) => (p.to_string(), r.to_string()),
        None => (String::new(), "about:client".to_string()),
    };
    // "" = the caller carried no explicit policy (fetch init unset, loaders,
    // XHR): fall back to the document's own policy (Referrer-Policy header or
    // <meta name=referrer>), else fetch_referer applies the spec default.
    let referrer_policy = if referrer_policy.is_empty() {
        resolve_document_referrer_policy(&state.borrow())
    } else {
        referrer_policy
    };
    tracing::debug!("op_fetch_url called: {} {} (intercept check pending)", method, url);

    let (init, body_is_base64) =
        match gather_fetch_parts(&state.borrow(), &url, &method, &headers_json) {
            Ok(parts) => parts,
            Err(json) => return Ok(json),
        };
    let FetchRequestInit {
        cookie_jar,
        in_flight,
        proxy_url,
        http_client,
        callbacks,
        document_url,
        intercept_tx,
    } = init;

    let mut override_url: Option<String> = None;
    let mut override_method: Option<String> = None;
    let mut override_headers: Option<HashMap<String, String>> = None;
    let mut override_body: Option<Vec<u8>> = None;
    if let Some(tx) = intercept_tx {
        let custom_headers: HashMap<String, String> = serde_json::from_str(&headers_json).unwrap_or_default();
        let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();
        let intercepted = InterceptedRequest {
            url: url.clone(),
            method: method.clone(),
            headers: custom_headers.clone(),
            resource_type: "Fetch".to_string(),
            resolver: resolve_tx,
        };
        if tx.send(intercepted).is_ok() {
            // Bounded wait: the bridge drains pauses between commands, so a
            // resolution normally lands within one command gap. When the
            // dispatch that triggered the fetch is itself parked on this
            // future (evaluate of a fetch expression), the resolution cannot
            // be delivered and the wait must expire instead of hanging the
            // command forever.
            let timeout_ms = INTERCEPT_RESOLUTION_TIMEOUT_MS.load(std::sync::atomic::Ordering::Relaxed);
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), resolve_rx).await {
                Ok(Ok(InterceptResolution::Fulfill { status, headers: h, body: b })) => {
                    let resp_headers: HashMap<String, String> = h;
                    // Byte-native: the synthetic body rides the same base64
                    // channel as real network responses (binary payloads
                    // survive; text is reconstructed on the JS side).
                    return Ok(serde_json::json!({
                        "status": status,
                        "bodyBase64": BASE64.encode(b),
                        "url": url,
                        "headers": resp_headers,
                    }).to_string());
                }
                Ok(Ok(InterceptResolution::Fail { reason })) => {
                    record_failed_fetch(&state.borrow(), &url, &method, reason.clone());
                    return Ok(serde_json::json!({
                        "status": 0,
                        "body": "",
                        "url": url,
                        "headers": {},
                        "blocked": true,
                        "error": reason,
                    }).to_string());
                }
                Ok(Ok(InterceptResolution::Continue { url: new_url, method: new_method, headers: new_headers, body: new_body })) => {
                    override_url = new_url;
                    override_method = new_method;
                    override_headers = new_headers;
                    override_body = new_body;
                }
                // Client answered nothing in time (or dropped the resolver):
                // fall through to the real request, i.e. Continue semantics.
                Ok(Err(_)) | Err(_) => {}
            }
        }
    }

    // Apply interception overrides (shadow the params for the rest of the op).
    // A Continue rewrite of the URL must pass the same SSRF / private-network
    // gate as the original request (checked above) and as redirects (checked
    // below). Without this re-validation a rewrite to an internal address would
    // bypass validate_fetch_url entirely.
    let url = if let Some(new_url) = override_url {
        if let Ok(parsed) = url::Url::parse(&new_url) {
            if let Err(reason) = validate_fetch_url(&parsed) {
                let error = format!("Intercept rewrite to forbidden URL blocked: {}", reason);
                record_failed_fetch(&state.borrow(), &new_url, &method, error.clone());
                return Ok(serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": new_url,
                    "blocked": true,
                    "error": error,
                }).to_string());
            }
        }
        new_url
    } else {
        url
    };
    let method = override_method.unwrap_or(method);
    // A Continue rewrite supplies raw request bytes (the CDP layer already
    // decoded the base64 wire form), so the base64 flag applies only to the
    // original body.
    let body_bytes: Vec<u8> = match override_body {
        Some(b) => b,
        None => {
            if body_is_base64 {
                BASE64.decode(&body).unwrap_or_default()
            } else {
                body.into_bytes()
            }
        }
    };
    let headers_json = match override_headers {
        Some(h) => serde_json::to_string(&h).unwrap_or(headers_json),
        None => headers_json,
    };

    let mut custom_headers: std::collections::HashMap<String, String> =
        serde_json::from_str(&headers_json).unwrap_or_default();
    // The out-of-band base64 marker must not leak to the wire or into the
    // preflight Access-Control-Request-Headers list.
    custom_headers.remove("__diting_body_b64");

    // url::Url::origin() normalizes default ports, so an explicit :443 still
    // compares same-origin (the old hand-rolled form did not).
    let initial_request_origin = request_origin(&url).unwrap_or_default();
    let page_origin = if origin.is_empty() { initial_request_origin } else { origin };

    let mut deps = FetchWalkDeps {
        url,
        method,
        custom_headers,
        body_bytes,
        page_origin,
        mode,
        credentials: FetchCredentials::parse(&credentials),
        cookie_jar,
        in_flight,
        http_client,
        proxy_url,
        document_url,
        referrer_policy,
        referrer_init,
        callbacks,
        failures: Vec::new(),
    };

    let outcome = match fetch_url_walk(&mut deps).await {
        Ok(outcome) => outcome,
        Err(e) => {
            replay_fetch_failures(&state.borrow(), &deps.failures);
            return Err(e);
        }
    };
    replay_fetch_failures(&state.borrow(), &deps.failures);
    if let Some(ev) = outcome.network.as_ref() {
        let request_id = record_fetch_network_event(&state.borrow(), ev);
        tracing::debug!(
            "op_fetch_url completed: {} {} ({} bytes, network event {})",
            ev.method,
            ev.url,
            ev.body_size,
            request_id,
        );
    }
    Ok(outcome.json)
}

/// The seven Referrer Policy tokens ("" is the "unset" marker, not a policy).
const REFERRER_POLICY_TOKENS: [&str; 7] = [
    "no-referrer",
    "no-referrer-when-downgrade",
    "origin",
    "origin-when-cross-origin",
    "strict-origin",
    "strict-origin-when-cross-origin",
    "unsafe-url",
];

/// Parse a comma-separated policy list (header value or meta content) per
/// Referrer Policy §"determine policy for token": tokens are
/// case-insensitive, invalid ones are skipped, the LAST valid token wins.
/// None = the whole value carries no policy.
pub(crate) fn last_valid_referrer_token(value: &str) -> Option<String> {
    let mut found = None;
    for tok in value.split(',') {
        let t = tok.trim().to_ascii_lowercase();
        if REFERRER_POLICY_TOKENS.contains(&t.as_str()) {
            found = Some(t);
        }
    }
    found
}

/// The document's own referrer policy (Referrer Policy §"Determine request's
/// Referrer Policy" base): a policy delivered via the Referrer-Policy
/// response header wins outright; otherwise the FIRST `<meta name=referrer>`
/// in tree order whose content yields a valid policy; "" = spec default.
pub(crate) fn resolve_referrer_policy_from(gs: &JsState, dom: &DomTree) -> String {
    if !gs.referrer_policy_header.is_empty() {
        return gs.referrer_policy_header.clone();
    }
    for nid in dom.query_selector_all("meta").unwrap_or_default() {
        let Some(n) = dom.get_node(nid) else { continue };
        let is_referrer_meta = n
            .get_attribute("name")
            .map(|v| v.to_ascii_lowercase() == "referrer")
            .unwrap_or(false);
        if !is_referrer_meta {
            continue;
        }
        let Some(content) = n.get_attribute("content").map(|v| v.to_string()) else {
            continue;
        };
        if let Some(p) = last_valid_referrer_token(&content) {
            return p;
        }
    }
    String::new()
}

/// Op-time wrapper: same resolution, from OpState's shared document state.
fn resolve_document_referrer_policy(state: &OpState) -> String {
    let shared = state.borrow::<SharedState>().clone();
    let gs = shared.borrow();
    match gs.dom.as_ref() {
        Some(dom) => resolve_referrer_policy_from(&gs, dom),
        None => gs.referrer_policy_header.clone(),
    }
}

/// Referer header for scripted fetch() requests per the Fetch standard
/// (obscura#875). `referrer_init` is the resolved RequestInit referrer: ""
/// sends none, "about:client" uses the document URL, anything else parses as
/// a URL (non-HTTP(S) or unparseable → no referrer, per spec). `policy` is
/// the validated referrerPolicy token; "" means the default
/// strict-origin-when-cross-origin. Same/cross-origin and the https→http
/// downgrade are judged against each hop's target so redirects re-strip.
/// Origin-only results keep the trailing "/" that our navigation_referrer
/// established (Chrome serializes without it; servers parse the header as a
/// URL either way).
pub(crate) fn fetch_referer(policy: &str, referrer_init: &str, document: &Url, target: &Url) -> String {
    let referrer = if referrer_init.is_empty() {
        return String::new();
    } else if referrer_init == "about:client" {
        document.clone()
    } else {
        match Url::parse(referrer_init) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => u,
            _ => return String::new(),
        }
    };
    let full = |u: &Url| -> String {
        let mut u = u.clone();
        u.set_fragment(None);
        let _ = u.set_username("");
        let _ = u.set_password(None);
        u.to_string()
    };
    let origin = format!("{}/", referrer.origin().ascii_serialization());
    let same_origin = referrer.origin() == target.origin();
    let downgrade = referrer.scheme() == "https" && target.scheme() != "https";
    let policy = if policy.is_empty() {
        "strict-origin-when-cross-origin"
    } else {
        policy
    };
    match policy {
        "no-referrer" => String::new(),
        "unsafe-url" => full(&referrer),
        "origin" => origin,
        "strict-origin" => {
            if downgrade {
                String::new()
            } else {
                origin
            }
        }
        "no-referrer-when-downgrade" => {
            if downgrade {
                String::new()
            } else {
                full(&referrer)
            }
        }
        "origin-when-cross-origin" | "strict-origin-when-cross-origin" => {
            if same_origin {
                full(&referrer)
            } else if downgrade && policy == "strict-origin-when-cross-origin" {
                String::new()
            } else {
                origin
            }
        }
        // JS validates the token; an unreachable value must not leak a URL.
        _ => String::new(),
    }
}

/// The transport half of op_fetch_url, shared verbatim by the deferred async
/// op and the sync-XHR op (obscura#908): client selection, CORS preflight,
/// the manual SSRF-revalidated redirect walk, scripted per-hop headers, the
/// terminal CORS check, the body cap, and response-callback dispatch. Touches
/// no OpState — everything stateful rides inside `deps` as Send clones;
/// failure events collect into `deps.failures` and the success network event
/// returns in the outcome, both for the driver to record once the walk
/// returns and OpState is reachable again.
async fn fetch_url_walk(
    deps: &mut FetchWalkDeps,
) -> Result<FetchWalkOutcome, deno_error::JsErrorBox> {
    let url = deps.url.clone();
    let method = deps.method.clone();
    let mode = deps.mode.clone();
    let page_origin = deps.page_origin.clone();
    let document_url = deps.document_url.clone();
    let referrer_policy = deps.referrer_policy.clone();
    let referrer_init = deps.referrer_init.clone();
    let custom_headers = std::mem::take(&mut deps.custom_headers);
    let body_bytes = std::mem::take(&mut deps.body_bytes);
    let credentials = deps.credentials;
    let cookie_jar = deps.cookie_jar.clone();
    let in_flight = deps.in_flight.clone();
    let http_client = deps.http_client.clone();
    let callbacks = deps.callbacks.clone();
    let proxy_url = deps.proxy_url.clone();

    // Pages use their context-scoped client so sequential runtimes never
    // share an async connection pool (upstream ab6fa0e, #453). The
    // process-wide cache remains the fallback for runtimes with no owning
    // HttpClient (e.g. a bare module-loader runtime).
    let client = match &http_client {
        Some(client) => client.request_client(&url).await,
        None => select_request_client(&url, proxy_url.as_deref())
            .await
            .map_err(deno_error::JsErrorBox::generic)?,
    };

    let req_method: reqwest::Method = method.parse().unwrap_or(reqwest::Method::GET);

    let is_cross_origin = request_origin(&url)
        .map(|initial| !page_origin.is_empty() && initial != page_origin)
        .unwrap_or(false);

    let unsafe_header_names = if is_cross_origin && mode == "cors" {
        cors_unsafe_request_header_names(&custom_headers)
    } else {
        Vec::new()
    };
    let needs_preflight = is_cross_origin
        && mode == "cors"
        && (!is_cors_safelisted_method(&req_method) || !unsafe_header_names.is_empty());

    if needs_preflight {
        let mut preflight_request = client
            .request(reqwest::Method::OPTIONS, &url)
            .header("Origin", &page_origin)
            .header("Access-Control-Request-Method", method.as_str());
        if !unsafe_header_names.is_empty() {
            preflight_request = preflight_request.header(
                "Access-Control-Request-Headers",
                unsafe_header_names.join(","),
            );
        }
        // The preflight rides the same plain-reqwest primary transport as the
        // main request, but for a while it was the one hop without the
        // legacy-TLS escape hatch: on a CBC-only endpoint the OPTIONS died in
        // the ClientHello and fetch failed before the main request — which
        // does retry on connect-stage failure — ever ran. The retry replays
        // the exact three preflight headers (no cookies: a CORS preflight is
        // never credentialed) and, like every other caller of the escape
        // hatch, a non-connect failure keeps the original error.
        let (pf_headers, allowed_origin, allow_credentials, preflight_status) =
            match preflight_request.send().await {
                Ok(p) => (
                    p.headers().clone(),
                    p.headers()
                        .get("access-control-allow-origin")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string(),
                    p.headers()
                        .get("access-control-allow-credentials")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string(),
                    p.status().as_u16(),
                ),
                Err(e) => {
                    let mut preflight_headers: HashMap<String, String> = HashMap::new();
                    preflight_headers.insert("Origin".to_string(), page_origin.clone());
                    preflight_headers.insert(
                        "Access-Control-Request-Method".to_string(),
                        method.clone(),
                    );
                    if !unsafe_header_names.is_empty() {
                        preflight_headers.insert(
                            "Access-Control-Request-Headers".to_string(),
                            unsafe_header_names.join(","),
                        );
                    }
                    let fallback = match http_client.as_ref() {
                        Some(hc) => match url::Url::parse(&url) {
                            Ok(u) => Some(
                                hc.scripted_fetch_fallback(
                                    &reqwest::Method::OPTIONS,
                                    &u,
                                    &e.to_string(),
                                    None,
                                    None,
                                    e.is_connect(),
                                    Some(&preflight_headers),
                                    false,
                                )
                                .await,
                            ),
                            Err(_) => None,
                        },
                        None => None,
                    };
                    match fallback {
                        Some(Ok(fr)) => {
                            let mut headers = reqwest::header::HeaderMap::new();
                            for (name, value) in &fr.headers {
                                if let (Ok(n), Ok(v)) = (
                                    reqwest::header::HeaderName::try_from(name.as_str()),
                                    reqwest::header::HeaderValue::try_from(value.as_str()),
                                ) {
                                    headers.insert(n, v);
                                }
                            }
                            (
                                headers,
                                fr.header("access-control-allow-origin").unwrap_or("").to_string(),
                                fr.header("access-control-allow-credentials").unwrap_or("").to_string(),
                                fr.status,
                            )
                        }
                        _ => {
                            let error = format!("CORS preflight failed: {}", e);
                            deps.failures.push((url.clone(), method.clone(), error.clone()));
                            return Err(deno_error::JsErrorBox::generic(error));
                        }
                    }
                }
            };

        // Every preflight rejection below is an early exit the JS side sees as
        // a rejected promise — each must leave a status-0 network event with
        // the reason, or the request vanishes from /network (the same ghost
        // the taobao punished-mtop report chased on the response-side gate).
        let mut reject_preflight = |message: String| -> deno_error::JsErrorBox {
            deps.failures.push((url.clone(), method.clone(), message.clone()));
            deno_error::JsErrorBox::generic(message)
        };

        // Fetch validates the preflight's HTTP status before its CORS headers;
        // the only observable difference is which error a 403-without-ACAO
        // preflight reports.
        if !(200..300).contains(&preflight_status) {
            return Err(reject_preflight(format!(
                "CORS preflight returned HTTP {}",
                preflight_status
            )));
        }
        if !cors_response_allows(credentials, &page_origin, &allowed_origin, &allow_credentials) {
            return Err(reject_preflight(format!(
                "CORS preflight: Origin '{}' not allowed by Access-Control-Allow-Origin '{}'",
                page_origin, allowed_origin
            )));
        }

        // The preflight must actually authorize the method and the unsafe
        // headers — a server that allows the origin but never listed the
        // method/headers does not consent to this request (obscura "enforce
        // CORS preflight permissions" fix, 04f0475).
        let allowed_methods = parse_cors_header_list(
            &pf_headers,
            "access-control-allow-methods",
        )
        .ok_or_else(|| {
            reject_preflight(
                "CORS preflight returned an invalid Access-Control-Allow-Methods value".to_string(),
            )
        })?;
        let allowed_headers = parse_cors_header_list(
            &pf_headers,
            "access-control-allow-headers",
        )
        .ok_or_else(|| {
            reject_preflight(
                "CORS preflight returned an invalid Access-Control-Allow-Headers value".to_string(),
            )
        })?;
        let credentialed = credentials == FetchCredentials::Include;
        if !preflight_allows_method(&req_method, &allowed_methods, credentialed) {
            return Err(reject_preflight(format!(
                "CORS preflight did not allow method '{}'",
                req_method
            )));
        }
        if let Some(name) = unsafe_header_names
            .iter()
            .find(|name| !preflight_allows_header(name, &allowed_headers, credentialed))
        {
            return Err(reject_preflight(format!(
                "CORS preflight did not allow request header '{}'",
                name
            )));
        }
    }

    // Follow redirects manually so the SSRF policy applies to every hop.
    // reqwest's auto-follow would bypass validate_fetch_url on the redirect
    // target and let an attacker-allowed origin 302 to http://127.0.0.1
    // (GHSA-8v6v-g4rh-jmcm).
    let mut current_url = url.clone();
    let mut current_method = req_method;
    let mut current_body = body_bytes;
    let mut redirects_followed: usize = 0;
    // Every hop target, in order — bounded by FETCH_REDIRECT_LIMIT because
    // the walk stops there. Surfaced on the success JSON (and the
    // redirect-shaped failures) so JS consumers can see where a fetch that
    // "went somewhere else" actually went; the walk stops at the limit so
    // the vec needs no separate cap.
    let mut redirect_chain: Vec<String> = Vec::new();
    // Fetch's redirect stripping (obscura#967 same hole): once the chain
    // crosses an origin, credentials scripted into custom headers stop
    // riding (browsers never forward Authorization to the redirect target),
    // and a 301/302/303 that downgrades the method to GET drops the request
    // body headers with it. Both sticky for the rest of the chain.
    let mut strip_credentials = false;
    let mut strip_body_headers = false;
    // Fetch's response tainting: once a cors-mode request touches a
    // cross-origin URL, every response in the chain gets the CORS check —
    // including the intermediate redirect responses (obscura#973 same hole:
    // only the terminal response used to be checked).
    let mut cors_tainted = request_origin(&url)
        .map(|o| o != page_origin)
        .unwrap_or(false);

    // Passive on_request observers (upstream #408): fire with the request as
    // the script shaped it, once, before the first hop goes out.
    if let Some(cbs) = callbacks.as_ref() {
        if cbs.has_request_callbacks().await {
            let sent_headers: HashMap<String, String> = custom_headers
                .iter()
                .map(|(k, v)| (k.to_lowercase(), v.clone()))
                .collect();
            let info = crate::diting_net::RequestInfo {
                url: url::Url::parse(&current_url).unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                method: current_method.to_string(),
                headers: sent_headers,
                resource_type: crate::diting_net::ResourceType::Fetch,
            };
            cbs.fire_request(&info).await;
        }
    }

    let mut response = loop {
        let mut req = client.request(current_method.clone(), &current_url);

        // Cross-origin and credentials are per-hop: a redirect can change
        // either answer (upstream b744b9b). Browsers send Origin on every
        // non-GET/HEAD request too — same-origin POSTs carry it (SolidStart
        // server functions 403 without it), so gate on method, not just domain.
        let current_is_cross_origin = request_origin(&current_url)
            .map(|o| o != page_origin)
            .unwrap_or(false);
        let method_needs_origin = current_method != reqwest::Method::GET
            && current_method != reqwest::Method::HEAD;
        if method_needs_origin || current_is_cross_origin {
            req = req.header("Origin", &page_origin);
        }

        let credentials_allowed = credentials.allows(&page_origin, &current_url);
        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    let cookie_header = jar.get_cookie_header(&parsed_url);
                    if !cookie_header.is_empty() {
                        req = req.header("Cookie", &cookie_header);
                    }
                }
            }
        }

        // Send browser-default headers on fetch()/XHR requests. The navigation
        // path sets these, but this op did not: scripted requests went out bare
        // (no UA, no Accept, no Fetch-Metadata / client-hint headers) and WAFs
        // that key on `sec-fetch-site: same-origin` (mcpservers.org /submit
        // 403s without it) rejected them. Honor explicit overrides.
        const DEFAULT_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
        let ua_override = custom_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.clone());
        let effective_ua = ua_override.clone().unwrap_or_else(|| DEFAULT_UA.to_string());
        let (sec_ch_ua, sec_ch_ua_platform) =
            crate::diting_net::client::derive_client_hints(&effective_ua);
        if ua_override.is_none() {
            req = req.header("User-Agent", &effective_ua);
        }
        for (name, value) in [
            ("sec-ch-ua", sec_ch_ua.clone()),
            ("sec-ch-ua-mobile", "?0".to_string()),
            ("sec-ch-ua-platform", sec_ch_ua_platform.clone()),
            ("accept", "*/*".to_string()),
        ] {
            if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case(name)) {
                req = req.header(name, value);
            }
        }
        // Fetch Metadata: derived per-hop because a redirect can change
        // same/cross-site. fetch()/XHR are always dest "empty".
        let sec_fetch_site = if current_is_cross_origin { "cross-site" } else { "same-origin" };
        let sec_fetch_mode = if mode.is_empty() { "cors" } else { mode.as_str() };
        for (name, value) in [
            ("sec-fetch-site", sec_fetch_site.to_string()),
            ("sec-fetch-mode", sec_fetch_mode.to_string()),
            ("sec-fetch-dest", "empty".to_string()),
        ] {
            if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case(name)) {
                req = req.header(name, value);
            }
        }

        // The Referer honors RequestInit's referrerPolicy/referrer when the
        // fetch carried them (obscura#875); the default
        // strict-origin-when-cross-origin trims per hop like client.rs.
        // Domain-whitelist APIs (e.g. AMap keys bound to a domain) reject
        // bare requests. Explicit Referer in fetch init still wins.
        if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case("referer"))
            && (!document_url.is_empty() || referrer_init != "about:client")
        {
            if let Ok(target) = Url::parse(&current_url) {
                let doc = Url::parse(&document_url).unwrap_or_else(|_| target.clone());
                let ref_val = fetch_referer(&referrer_policy, &referrer_init, &doc, &target);
                if !ref_val.is_empty() {
                    req = req.header(reqwest::header::REFERER, ref_val);
                }
            }
        }

        // Per-hop effective scripted headers: the first hop sends everything
        // as the script shaped it; after a credential-triggering or
        // method-downgrading redirect the stripped subset rides instead.
        let effective_headers: HashMap<String, String> = custom_headers
            .iter()
            .filter(|(k, _)| {
                let lower = k.to_ascii_lowercase();
                if strip_credentials
                    && matches!(
                        lower.as_str(),
                        "authorization" | "proxy-authorization" | "cookie"
                    )
                {
                    return false;
                }
                if strip_body_headers
                    && matches!(
                        lower.as_str(),
                        "content-type"
                            | "content-length"
                            | "content-encoding"
                            | "content-language"
                            | "content-location"
                    )
                {
                    return false;
                }
                true
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in &effective_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        if !current_body.is_empty() {
            req = req.body(current_body.clone());
        }

        if let Some(ref counter) = in_flight {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if let Some(ref counter) = in_flight {
                    counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                // Scripted fetch()/XHR walked this far on a raw reqwest
                // client with no retry of its own — the one subresource path
                // the Tier2 fallback didn't cover (the g.alicdn.com shape:
                // plain rustls dies on the handshake, the stealth stack's
                // BoringSSL connects). A GET/HEAD transport failure gets the
                // same one-shot legacy-TLS escape hatch the script and
                // stylesheet loaders use; any other method rides only when
                // the failure is connect-stage (DNS/TCP/TLS — the request
                // provably never left the machine), so a POST form submit
                // cannot double-submit (taobao seller-backend shape:
                // CBC-only endpoints killed every publish POST at the
                // handshake). The retry also rebuilds the hop's scripted
                // header set (Origin, Referer, Fetch-Metadata, client hints)
                // and the credentials policy, so the legacy transport sends
                // the same request, not a bare one — Referer-checking WAFs
                // 403 the bare shape even after the handshake succeeds.
                let fallback_ctype = effective_headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone());
                let fallback = match http_client.as_ref() {
                    Some(hc)
                        if current_method == reqwest::Method::GET
                            || current_method == reqwest::Method::HEAD
                            || e.is_connect() =>
                    {
                        match url::Url::parse(&current_url) {
                            Ok(u) => {
                                let mut fallback_headers = effective_headers.clone();
                                fn header_set(
                                    headers: &HashMap<String, String>,
                                    name: &str,
                                ) -> bool {
                                    headers.keys().any(|k| k.eq_ignore_ascii_case(name))
                                }
                                if (method_needs_origin || current_is_cross_origin)
                                    && !header_set(&fallback_headers, "origin")
                                {
                                    fallback_headers
                                        .insert("Origin".into(), page_origin.clone());
                                }
                                if !header_set(&fallback_headers, "user-agent") {
                                    fallback_headers
                                        .insert("User-Agent".into(), effective_ua.clone());
                                }
                                for (name, value) in [
                                    ("sec-ch-ua", sec_ch_ua.clone()),
                                    ("sec-ch-ua-mobile", "?0".to_string()),
                                    ("sec-ch-ua-platform", sec_ch_ua_platform.clone()),
                                    ("accept", "*/*".to_string()),
                                    (
                                        "sec-fetch-site",
                                        if current_is_cross_origin {
                                            "cross-site"
                                        } else {
                                            "same-origin"
                                        }
                                        .to_string(),
                                    ),
                                    (
                                        "sec-fetch-mode",
                                        if mode.is_empty() {
                                            "cors"
                                        } else {
                                            mode.as_str()
                                        }
                                        .to_string(),
                                    ),
                                    ("sec-fetch-dest", "empty".to_string()),
                                ] {
                                    if !header_set(&fallback_headers, name) {
                                        fallback_headers.insert(name.to_string(), value);
                                    }
                                }
                                if (!document_url.is_empty()
                                    || referrer_init != "about:client")
                                    && !header_set(&fallback_headers, "referer")
                                {
                                    let doc = Url::parse(&document_url).unwrap_or_else(|_| u.clone());
                                    let referrer =
                                        fetch_referer(&referrer_policy, &referrer_init, &doc, &u);
                                    if !referrer.is_empty() {
                                        fallback_headers.insert("Referer".into(), referrer);
                                    }
                                }
                                Some(
                                    hc.scripted_fetch_fallback(
                                        &current_method,
                                        &u,
                                        &e.to_string(),
                                        (!current_body.is_empty()).then_some(current_body.as_slice()),
                                        fallback_ctype.as_deref(),
                                        e.is_connect(),
                                        Some(&fallback_headers),
                                        credentials_allowed,
                                    )
                                    .await,
                                )
                            }
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };
                match fallback {
                    Some(Ok(buffered)) => break OpFetchOutcome::Buffered(buffered),
                    Some(Err(fallback_err)) => {
                        deps.failures.push((current_url.clone(), current_method.as_str().to_string(), fallback_err.to_string()));
                        return Err(deno_error::JsErrorBox::generic(fallback_err.to_string()))
                    }
                    None => {
                        deps.failures.push((current_url.clone(), current_method.as_str().to_string(), e.to_string()));
                        return Err(deno_error::JsErrorBox::generic(e.to_string()))
                    }
                }
            }
        };

        if let Some(ref counter) = in_flight {
            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }

        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
                        if let Ok(s) = val.to_str() {
                            jar.set_cookie(s, &parsed_url);
                        }
                    }
                }
            }
        }

        if !resp.status().is_redirection() {
            break OpFetchOutcome::Live(resp);
        }

        let location_header = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location_header else {
            // 3xx without a Location header is not actually a redirect.
            break OpFetchOutcome::Live(resp);
        };

        let base = match url::Url::parse(&current_url) {
            Ok(b) => b,
            Err(_) => break OpFetchOutcome::Live(resp),
        };
        let next_url = match base.join(&location) {
            Ok(u) => u,
            Err(_) => break OpFetchOutcome::Live(resp),
        };

        // Re-validate every redirect target against the SSRF policy.
        if let Err(reason) = validate_fetch_url(&next_url) {
            let error = format!("Redirect to forbidden URL blocked: {}", reason);
            deps.failures.push((next_url.to_string(), current_method.as_str().to_string(), error.clone()));
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": next_url.to_string(),
                    "headers": {},
                    "blocked": true,
                    "error": error,
                    "redirect_chain": redirect_chain,
                })
                .to_string(),
                network: None,
            });
        }

        redirect_chain.push(next_url.to_string());
        redirects_followed += 1;
        if redirects_followed > FETCH_REDIRECT_LIMIT {
            let error = format!("Too many redirects (>{})", FETCH_REDIRECT_LIMIT);
            deps.failures.push((next_url.to_string(), current_method.as_str().to_string(), error.clone()));
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": next_url.to_string(),
                    "headers": {},
                    "blocked": true,
                    "error": error,
                    "redirect_chain": redirect_chain,
                })
                .to_string(),
                network: None,
            });
        }

        // The CORS check applies to every tainted response, redirect
        // responses included: a cross-origin server must authorize the
        // redirect itself before the follow happens.
        if mode == "cors" && cors_tainted {
            let allowed = resp
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let allow_credentials = resp
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
                let error = format!(
                    "CORS error: redirect to '{}' blocked: Origin '{}' not in Access-Control-Allow-Origin '{}'",
                    next_url, page_origin, allowed
                );
                deps.failures.push((current_url.clone(), current_method.as_str().to_string(), error.clone()));
                return Ok(FetchWalkOutcome {
                    json: serde_json::json!({
                        "status": 0,
                        "body": "",
                        "url": url,
                        "headers": {},
                        "corsBlocked": true,
                        "corsError": error,
                        "redirect_chain": redirect_chain,
                    })
                    .to_string(),
                    network: None,
                });
            }
        }
        cors_tainted = cors_tainted
            || request_origin(next_url.as_str())
                .map(|o| o != page_origin)
                .unwrap_or(false);

        // Browser semantics: 301/302/303 downgrade to GET with no body (303
        // unconditionally; 301/302 only when the method actually changes —
        // a GET→GET redirect keeps its header list). 307/308 preserve
        // method and body.
        let status_code = resp.status().as_u16();
        let method_downgrades = status_code == 303
            || ((status_code == 301 || status_code == 302)
                && current_method != reqwest::Method::GET
                && current_method != reqwest::Method::HEAD);
        if method_downgrades {
            current_method = reqwest::Method::GET;
            current_body.clear();
            strip_body_headers = true;
        }
        if request_origin(next_url.as_str()) != request_origin(&current_url) {
            strip_credentials = true;
        }

        current_url = next_url.to_string();
    };

    // The legacy fallback resolved (and revalidated) its redirects inside
    // the HttpClient; adopt its final URL so the CORS check and the passive
    // observers below see where the content actually came from.
    let (status, resp_headers, buffered_body): (
        u16,
        std::collections::HashMap<String, String>,
        Option<Vec<u8>>,
    ) = match &response {
        OpFetchOutcome::Live(r) => {
            let status = r.status().as_u16();
            let headers = crate::diting_net::collect_response_headers(r.headers());
            (status, headers, None)
        }
        OpFetchOutcome::Buffered(r) => {
            current_url = r.url.to_string();
            (r.status, r.headers.clone(), Some(r.body.clone()))
        }
    };

    let final_is_cross_origin = request_origin(&current_url)
        .map(|o| o != page_origin)
        .unwrap_or(false);
    if final_is_cross_origin && mode == "cors" {
        let allowed = resp_headers
            .get("access-control-allow-origin")
            .map(|s| s.as_str())
            .unwrap_or("");
        let allow_credentials = resp_headers
            .get("access-control-allow-credentials")
            .map(|s| s.as_str())
            .unwrap_or("");

        if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
            let error = if credentials == FetchCredentials::Include {
                format!("CORS error: credentialed request requires Access-Control-Allow-Origin '{}' and Access-Control-Allow-Credentials 'true'", page_origin)
            } else {
                format!("CORS error: Origin '{}' not in Access-Control-Allow-Origin '{}'", page_origin, allowed)
            };
            deps.failures.push((current_url.clone(), current_method.as_str().to_string(), error.clone()));
            return Ok(FetchWalkOutcome {
                json: serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": url,
                    "headers": {},
                    "corsBlocked": true,
                    "corsError": error,
                })
                .to_string(),
                network: None,
            });
        }
    }

    // Cap the buffered body (upstream #581): the retained-for-CDP limits
    // above only gate the cache, never the allocation, and everything
    // downstream (utf8-lossy, base64) copies the full buffer again.
    // Content-Length is checked before reading; a lying or absent header
    // still runs into the per-chunk check while streaming.
    let body_limit = fetch_body_byte_limit();
    if let Some(len) = resp_headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        if len > body_limit {
            return Err(deno_error::JsErrorBox::generic(format!(
                "fetch response body too large: content-length {} exceeds limit {} bytes",
                len, body_limit
            )));
        }
    }
    let resp_bytes: Vec<u8> = match buffered_body {
        // The fallback pre-buffered its body; the per-chunk cap the
        // streaming path enforces applies to it as one shot.
        Some(bytes) => {
            if bytes.len() > body_limit {
                return Err(deno_error::JsErrorBox::generic(format!(
                    "fetch response body exceeded limit of {} bytes",
                    body_limit
                )));
            }
            bytes
        }
        None => {
            // buffered_body is Some exactly for the Buffered outcome, so
            // streaming here implies Live.
            let live = match &mut response {
                OpFetchOutcome::Live(r) => r,
                OpFetchOutcome::Buffered(_) => {
                    unreachable!("buffered bodies never take the streaming path")
                }
            };
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = live
                .chunk()
                .await
                .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?
            {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > body_limit {
                    return Err(deno_error::JsErrorBox::generic(format!(
                        "fetch response body exceeded limit of {} bytes",
                        body_limit
                    )));
                }
            }
            bytes
        }
    };
    // Chromium DevTools body policy (Chrome 152 verified, obscura #791): a
    // declared non-UTF-8 charset (GBK) decodes to text with base64Encoded=false;
    // opaque or undecodable bodies travel base64 byte-exact.
    let stored_text = crate::diting_net::decode_devtools_body(
        &resp_bytes,
        resp_headers.get("content-type").map(|s| s.as_str()),
    );
    let resp_body_base64 = BASE64.encode(&resp_bytes);

    // Hand the success network event to the driver (recorded once the walk
    // returns and OpState is reachable again), then fire the passive
    // on_response observers.
    let network = FetchNetworkEvent {
        url: url.clone(),
        method: method.clone(),
        status,
        response_headers: resp_headers.clone(),
        body_size: resp_bytes.len(),
        stored_text: stored_text.clone(),
        resp_body_base64: resp_body_base64.clone(),
    };

    if let Some(cbs) = callbacks.as_ref() {
        if cbs.has_response_callbacks().await {
            let info = crate::diting_net::RequestInfo {
                url: url::Url::parse(&current_url)
                    .unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                method: method.clone(),
                headers: resp_headers.clone(),
                resource_type: crate::diting_net::ResourceType::Fetch,
            };
            let net_resp = crate::diting_net::Response {
                url: url::Url::parse(&current_url)
                    .unwrap_or_else(|_| url::Url::parse("about:blank").unwrap()),
                status,
                headers: resp_headers.clone(),
                body: resp_bytes.to_vec(),
                redirected_from: Vec::new(),
            };
            cbs.fire_response(&info, &net_resp).await;
        }
    }

    Ok(FetchWalkOutcome {
        json: serde_json::json!({
            "status": status,
            "body": stored_text.unwrap_or_default(),
            "bodyBase64": resp_body_base64,
            "url": url,
            "final_url": current_url,
            "redirected": redirects_followed > 0,
            "redirect_chain": redirect_chain,
            "headers": resp_headers,
        })
        .to_string(),
        network: Some(network),
    })
}

pub(crate) fn glob_match(pattern: &str, url: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('*') && pattern.ends_with('*') {
        return url.contains(&pattern[1..pattern.len() - 1]);
    }
    if pattern.starts_with('*') {
        return url.ends_with(&pattern[1..]);
    }
    if pattern.ends_with('*') {
        return url.starts_with(&pattern[..pattern.len() - 1]);
    }
    url == pattern
}

/// Record a `status: 0` network event for a fetch that never produced a
/// servable response — SSRF-blocked, URL-blocklisted, CORS-refused, or dead
/// at the transport layer. The success path records via
/// `record_fetch_network_event`; without this companion every early exit
/// vanished from the session /network log, and a page whose API calls all
/// died here read as "never issued a request" — exactly the ghost the taobao
/// shop-SPA report chased (punished mtop XHRs are CORS-refused after the
/// body arrives, so the whole call site went dark).
fn record_failed_fetch(
    state: &OpState,
    url: &str,
    method: &str,
    error: String,
) {
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    gs.network_response_body_counter += 1;
    // Same `fetch-{N}` id space as the success path (no body is stored under
    // it — there is none to retrieve); keeps ids unique across interleaved
    // failures and successes.
    let request_id = format!("fetch-{}", gs.network_response_body_counter);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    gs.js_network_events.push(JsNetworkEvent {
        request_id,
        url: url.to_string(),
        method: method.to_string(),
        status: 0,
        response_headers: HashMap::new(),
        body_size: 0,
        timestamp,
        error: Some(error),
    });
    const MAX_JS_NETWORK_EVENTS: usize = 4096;
    if gs.js_network_events.len() > MAX_JS_NETWORK_EVENTS {
        let overflow = gs.js_network_events.len() - MAX_JS_NETWORK_EVENTS;
        gs.js_network_events.drain(0..overflow);
    }
}

/// Synchronous twin of op_fetch_url (obscura#908): XHR.open(..., false) +
/// send() must issue the request and return with status populated, with no
/// event-loop turn in between. deno_core async ops only resolve when the
/// embedding pumps the event loop after the eval returns — impossible while
/// JS holds the thread inside send() — so the shared walk runs on a worker
/// thread with its own current-thread tokio runtime and this op blocks on
/// the reply channel.
///
/// Two divergences from the async path, both inherent to holding the JS
/// thread:
/// - CDP Fetch interception is skipped: a resolution cannot be delivered
///   while the dispatch that raised the request is parked here (the same
///   reason the async path's interception wait is bounded).
/// - The shared reqwest client is touched from a second runtime.
///   Per-request connection tasks spawn on the driving runtime, so this is
///   sound; a pooled keep-alive connection left by a dead temporary runtime
///   can error transiently ("dispatch task is gone") and surface as a
///   network error — rare on this legacy path.
const SYNC_XHR_TIMEOUT_MS: u64 = 120_000;

#[op2]
#[string]
fn op_fetch_url_sync(
    state: &OpState,
    #[string] url: String,
    #[string] method: String,
    #[string] headers_json: String,
    #[string] body: String,
    #[string] origin: String,
    #[string] mode: String,
    #[string] credentials: String,
) -> Result<String, deno_error::JsErrorBox> {
    let (init, body_is_base64) = gather_fetch_parts(state, &url, &method, &headers_json)
        .map_err(deno_error::JsErrorBox::generic)?;
    let FetchRequestInit {
        cookie_jar,
        in_flight,
        proxy_url,
        http_client,
        callbacks,
        document_url,
        ..
    } = init;

    let body_bytes = if body_is_base64 {
        BASE64.decode(&body).unwrap_or_default()
    } else {
        body.into_bytes()
    };
    let mut custom_headers: std::collections::HashMap<String, String> =
        serde_json::from_str(&headers_json).unwrap_or_default();
    custom_headers.remove("__diting_body_b64");

    let initial_request_origin = request_origin(&url).unwrap_or_default();
    let page_origin = if origin.is_empty() { initial_request_origin } else { origin };

    let mut deps = FetchWalkDeps {
        url,
        method,
        custom_headers,
        body_bytes,
        page_origin,
        mode,
        credentials: FetchCredentials::parse(&credentials),
        cookie_jar,
        in_flight,
        http_client,
        proxy_url,
        document_url,
        // XHR has no referrerPolicy surface (Fetch-only RequestInit); the
        // document's own policy (header / <meta name=referrer>) applies.
        referrer_policy: resolve_document_referrer_policy(state),
        referrer_init: "about:client".to_string(),
        callbacks,
        failures: Vec::new(),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("sync-xhr".to_string())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(fetch_url_walk(&mut deps)),
                Err(e) => Err(deno_error::JsErrorBox::generic(format!(
                    "sync XHR transport runtime: {}",
                    e
                ))),
            };
            let failures = deps.failures;
            let _ = tx.send((result, failures));
        })
        .map_err(|e| deno_error::JsErrorBox::generic(format!("sync XHR worker: {}", e)))?;

    let (result, failures) =
        match rx.recv_timeout(std::time::Duration::from_millis(SYNC_XHR_TIMEOUT_MS)) {
            Ok(pair) => pair,
            Err(_) => {
                return Err(deno_error::JsErrorBox::generic(
                    "Synchronous XMLHttpRequest timed out or its worker died",
                ));
            }
        };
    replay_fetch_failures(state, &failures);
    let outcome = result?;
    if let Some(ev) = outcome.network.as_ref() {
        record_fetch_network_event(state, ev);
    }
    Ok(outcome.json)
}

/// Also applied by the ES module loader (obscura #849): dynamic import() is
/// as page-reachable as fetch(), so it answers to the same scheme and
/// private-network policy. The cached client these paths share never
/// auto-follows redirects, so validating the resolved specifier covers
/// every hop a fetch can actually take.
pub(crate) fn validate_fetch_url(url: &url::Url) -> Result<(), String> {
    let scheme = url.scheme();
    // file:// is rejected up front for page-reachable fetch/XHR, matching the
    // deny-by-default navigation posture (upstream obscura #708: the old gate
    // allowed the scheme through and short-circuited the SSRF checks; the
    // transports couldn't actually fetch it, but the inconsistency leaked an
    // "allowed" signal to probing scripts).
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "Forbidden URL scheme '{}' - only http and https are allowed",
            scheme
        ));
    }

    if crate::diting_net::env_allows_private_network() {
        return Ok(());
    }

    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(ip) => {
                // Shared deny-set with navigation (is_forbidden_base), not a
                // local re-listing: the hand-rolled loopback/RFC1918 checks
                // here missed the IANA special-purpose ranges (198.18.0.0/15
                // benchmarking, 100.64/10 CGNAT metadata, 0.0.0.0/8) and the
                // embedded-IPv4 forms (mapped, 6to4, NAT64), so a page could
                // fetch addresses the navigation gate blocks (obscura #852
                // family). Scoped allow-network subtraction is built into
                // is_forbidden_ip.
                if crate::diting_net::client::is_forbidden_ip(std::net::IpAddr::V4(ip)) {
                    return Err(format!(
                        "Access to private/internal IP address {} is not allowed",
                        ip
                    ));
                }
            }
            url::Host::Ipv6(ip) => {
                if crate::diting_net::client::is_forbidden_ip(std::net::IpAddr::V6(ip)) {
                    return Err(format!(
                        "Access to private/internal IPv6 address {} is not allowed",
                        ip
                    ));
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(format!(
                        "Access to localhost domain '{}' is not allowed",
                        domain
                    ));
                }
            }
        }
    }

    Ok(())
}

#[op2]
#[string]
fn op_get_cookies(state: &OpState) -> String {
    let gs = state.borrow::<SharedState>().clone();
    let gs = gs.borrow();
    let jar = match &gs.cookie_jar {
        Some(j) => j,
        None => return String::new(),
    };
    let url = match url::Url::parse(&gs.url) {
        Ok(u) => u,
        Err(_) => return String::new(),
    };
    jar.get_js_visible_cookies(&url)
}

#[op2(fast)]
fn op_set_cookie(state: &OpState, #[string] cookie_str: &str) {
    let gs = state.borrow::<SharedState>().clone();
    let gs = gs.borrow();
    let jar = match &gs.cookie_jar {
        Some(j) => j,
        None => return,
    };
    let url = match url::Url::parse(&gs.url) {
        Ok(u) => u,
        Err(_) => return,
    };
    jar.set_cookie_from_js(cookie_str, &url);
}

// localStorage persistence (obscura#629 class): the bootstrap store is a
// plain JS map, so logins/flags a page keeps in localStorage died with the
// process. The store now loads through op_storage_read on first access and
// flushes (debounced JS-side) through op_storage_write; the authoritative
// copy lives in this process-global map, mirrored to one JSON file per
// origin under AGINXBROWSER_STORAGE_DIR (falling back to the cookie store
// dir). sessionStorage deliberately stays memory-only — per-tab lifetime is
// the spec'd behavior.
static LOCAL_STORAGE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, HashMap<String, String>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
static LOCAL_STORAGE_LOADED: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

fn storage_file(origin: &str) -> Option<std::path::PathBuf> {
    if crate::config::ephemeral() {
        return None;
    }
    let dir = std::env::var("AGINXBROWSER_STORAGE_DIR")
        .ok()
        .or_else(|| std::env::var("AGINXBROWSER_COOKIE_STORE_DIR").ok())
        .or_else(|| crate::config::app_data_dir().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string());
    let mut sanitized = String::with_capacity(origin.len());
    for c in origin.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    // FNV-1a suffix: sanitized names collide ("a:b" vs "a_b"); the hash does not.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in origin.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(
        std::path::Path::new(&dir)
            .join("localStorage")
            .join(format!("{sanitized}-{:016x}.json", hash)),
    )
}

fn storage_load_locked(origin: &str) {
    let mut loaded = LOCAL_STORAGE_LOADED.lock().unwrap();
    if !loaded.insert(origin.to_string()) {
        return;
    }
    let Some(path) = storage_file(origin) else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(entries) = serde_json::from_str::<HashMap<String, String>>(&raw) else {
        return;
    };
    LOCAL_STORAGE
        .lock()
        .unwrap()
        .insert(origin.to_string(), entries);
}

#[op2]
#[string]
fn op_storage_read(#[string] origin: &str) -> String {
    storage_load_locked(origin);
    let map = LOCAL_STORAGE.lock().unwrap();
    let entries = map.get(origin);
    serde_json::to_string(&entries.unwrap_or(&HashMap::new())).unwrap_or_else(|_| "{}".to_string())
}

#[op2(fast)]
fn op_storage_write(#[string] origin: &str, #[string] json: &str) {
    let Ok(entries) = serde_json::from_str::<HashMap<String, String>>(json) else {
        return;
    };
    // Load-first: a write for a never-read origin must not be clobbered by
    // the on-disk copy when the reader side initializes later.
    storage_load_locked(origin);
    LOCAL_STORAGE
        .lock()
        .unwrap()
        .insert(origin.to_string(), entries);
    let Some(path) = storage_file(origin) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            // An unwritable store must be loud (obscura#855 item 1 shape):
            // silently returning here loses every subsequent localStorage
            // write with zero diagnostics.
            tracing::warn!(
                "localStorage persist: create_dir_all({}) failed: {}",
                parent.display(),
                e
            );
            return;
        }
    }
    let payload = LOCAL_STORAGE
        .lock()
        .unwrap()
        .get(origin)
        .map(|entries| serde_json::to_string(entries).unwrap_or_else(|_| "{}".to_string()));
    let Some(payload) = payload else { return };
    // tmp + rename so a mid-write kill never leaves a truncated store;
    // 0600 to match the cookie store — this file carries login tokens too
    // (the xinzao-class session shape).
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    let write_ok = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(payload.as_bytes()))
            .is_ok()
    };
    // Windows has no mode bits; the file lives in the per-user app-data
    // directory (config::app_data_dir), whose ACLs already scope it to the
    // user.
    #[cfg(not(unix))]
    let write_ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(payload.as_bytes()))
        .is_ok();
    if write_ok {
        if let Err(e) = std::fs::rename(&tmp, &path) {
            tracing::warn!("localStorage persist: rename to {} failed: {}", path.display(), e);
        }
    } else {
        tracing::warn!("localStorage persist: write to {} failed", tmp.display());
    }
}

#[cfg(test)]
pub(crate) fn reset_local_storage_for_tests() {
    LOCAL_STORAGE.lock().unwrap().clear();
    LOCAL_STORAGE_LOADED.lock().unwrap().clear();
}

#[op2(fast)]
fn op_navigate(state: &OpState, #[string] url: &str, #[string] method: &str, #[string] body: &str) {
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    // Only queue the navigation — do NOT move the realm URL here. The URL is
    // written on commit (init_js → set_url from the page's committed URL).
    // Moving it early let synchronous JS between the assignment and the
    // actual navigation read and write another origin's cookies through
    // document.cookie, whose ops derive the domain from this URL
    // (SOP bypass, obscura #940).
    gs.pending_navigation = Some((url.to_string(), method.to_string(), body.to_string()));
}

#[op2(async(deferred), fast)]
async fn op_sleep(#[number] millis: u64) {
    // Reactor-less contexts (plain #[test] isolates): a tokio timer would
    // panic mid-poll inside v8, which aborts the whole test binary. Resolve
    // at the next event-loop checkpoint instead — production always runs
    // under a reactor and never takes this path.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

// Records a binding call from page JS. The CDP layer drains this queue
// after every dispatch and emits one `Runtime.bindingCalled` event per
// entry, that's how puppeteer's `page.exposeFunction` callbacks fire.
#[op2(fast)]
fn op_binding_called(state: &OpState, #[string] name: &str, #[string] payload: &str) {
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    gs.pending_binding_calls.push((name.to_string(), payload.to_string()));
    // The CDP layer drains this after every dispatch, but a page can loop
    // calling an exposed binding many times inside one long evaluate — same
    // unbounded-queue shape as js_network_events (obscura #705 class).
    const MAX_PENDING_BINDING_CALLS: usize = 4096;
    if gs.pending_binding_calls.len() > MAX_PENDING_BINDING_CALLS {
        let overflow = gs.pending_binding_calls.len() - MAX_PENDING_BINDING_CALLS;
        gs.pending_binding_calls.drain(0..overflow);
    }
}

/// Real WebCrypto `crypto.subtle.digest`. `algorithm` is the SubtleCrypto
/// algorithm name (`SHA-1` / `SHA-256` / `SHA-384` / `SHA-512`); unknown
/// names fall through to SHA-256 to match the previous JS fallback. Returns
/// the raw digest bytes so the JS shim can hand them back as an ArrayBuffer.
#[op2]
#[buffer]
fn op_subtle_digest(#[string] algorithm: &str, #[buffer] data: &[u8]) -> Vec<u8> {
    use sha1::Digest as _;
    let alg = algorithm.to_ascii_uppercase();
    match alg.as_str() {
        "SHA-1" => sha1::Sha1::digest(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        "SHA-512/224" => sha2::Sha512_224::digest(data).to_vec(),
        "SHA-512/256" => sha2::Sha512_256::digest(data).to_vec(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// WebCrypto (crypto.subtle) secret-key primitives.
//
// These ops are stateless. The JS shim in bootstrap.js owns the CryptoKey
// objects and their raw key bytes; it hands the bytes plus normalized algorithm
// parameters to these ops for each operation. Only secret-key algorithms live
// here (HMAC, AES-GCM/CBC/CTR, PBKDF2, HKDF); public-key algorithms are rejected
// in the shim. A fallible op returns a JsErrorBox that the shim turns into the
// appropriate DOMException (OperationError for a bad tag or padding, etc.).
// ---------------------------------------------------------------------------

fn crypto_err(msg: impl std::fmt::Display) -> deno_error::JsErrorBox {
    deno_error::JsErrorBox::generic(msg.to_string())
}

/// HMAC sign. `hash` is a normalized SubtleCrypto hash name; any key length is
/// accepted (HMAC pads or hashes the key per RFC 2104). Returns the MAC bytes;
/// the shim does the constant-time-insensitive compare for `verify`.
#[op2]
#[buffer]
fn op_subtle_hmac(
    #[string] hash: &str,
    #[buffer] key: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use hmac::{Hmac, Mac};
    macro_rules! run {
        ($d:ty) => {{
            let mut mac = Hmac::<$d>::new_from_slice(key).map_err(crypto_err)?;
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }};
    }
    Ok(match hash {
        "SHA-1" => run!(sha1::Sha1),
        "SHA-256" => run!(sha2::Sha256),
        "SHA-384" => run!(sha2::Sha384),
        "SHA-512" => run!(sha2::Sha512),
        _ => return Err(crypto_err("unsupported HMAC hash")),
    })
}

/// AES-GCM encrypt/decrypt. WebCrypto's ciphertext carries the auth tag
/// appended, which is exactly RustCrypto's combined form, so this maps 1:1.
/// Restricted to a 96-bit IV and 128-bit tag (the WebCrypto defaults and the
/// overwhelming majority of real usage); the shim rejects other tag lengths.
#[op2]
#[buffer]
fn op_subtle_aes_gcm(
    encrypt: bool,
    #[buffer] key: &[u8],
    #[buffer] iv: &[u8],
    #[buffer] aad: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::aes::{Aes192, Aes256};
    use aes_gcm::{AesGcm, Nonce};
    type Aes192Gcm = AesGcm<Aes192, aes_gcm::aead::consts::U12>;
    type Aes256Gcm = AesGcm<Aes256, aes_gcm::aead::consts::U12>;

    if iv.len() != 12 {
        return Err(crypto_err("AES-GCM requires a 96-bit (12-byte) IV"));
    }
    let nonce = Nonce::from_slice(iv);
    macro_rules! run {
        ($ty:ty) => {{
            let cipher = <$ty>::new_from_slice(key).map_err(crypto_err)?;
            if encrypt {
                cipher
                    .encrypt(nonce, Payload { msg: data, aad })
                    .map_err(|_| crypto_err("AES-GCM encryption failed"))?
            } else {
                cipher
                    .decrypt(nonce, Payload { msg: data, aad })
                    .map_err(|_| crypto_err("AES-GCM decryption failed: authentication tag mismatch"))?
            }
        }};
    }
    Ok(match key.len() {
        16 => run!(aes_gcm::Aes128Gcm),
        24 => run!(Aes192Gcm),
        32 => run!(Aes256Gcm),
        _ => return Err(crypto_err("AES-GCM key must be 128, 192, or 256 bits")),
    })
}

/// AES-CBC encrypt/decrypt with PKCS#7 padding (the only padding WebCrypto
/// AES-CBC uses) and a 16-byte IV.
#[op2]
#[buffer]
fn op_subtle_aes_cbc(
    encrypt: bool,
    #[buffer] key: &[u8],
    #[buffer] iv: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use cbc::cipher::block_padding::Pkcs7;
    use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
    use cbc::{Decryptor, Encryptor};

    if iv.len() != 16 {
        return Err(crypto_err("AES-CBC requires a 16-byte IV"));
    }
    macro_rules! run {
        ($cipher:ty) => {{
            if encrypt {
                Encryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(crypto_err)?
                    .encrypt_padded_vec_mut::<Pkcs7>(data)
            } else {
                Decryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(crypto_err)?
                    .decrypt_padded_vec_mut::<Pkcs7>(data)
                    .map_err(|_| crypto_err("AES-CBC decryption failed: invalid padding"))?
            }
        }};
    }
    Ok(match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        32 => run!(aes::Aes256),
        _ => return Err(crypto_err("AES-CBC key must be 128, 192, or 256 bits")),
    })
}

/// AES-CTR. Encrypt and decrypt are the same keystream XOR. `counter_length` is
/// the WebCrypto counter width in bits; it selects the RustCrypto CTR flavor so
/// only the low `counter_length` bits of the 16-byte block increment.
#[op2]
#[buffer]
fn op_subtle_aes_ctr(
    #[buffer] key: &[u8],
    #[buffer] counter: &[u8],
    counter_length: u32,
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use ctr::cipher::{KeyIvInit, StreamCipher};

    if counter.len() != 16 {
        return Err(crypto_err("AES-CTR requires a 16-byte counter block"));
    }
    let mut buf = data.to_vec();
    macro_rules! run {
        ($ty:ty) => {{
            <$ty>::new_from_slices(key, counter)
                .map_err(crypto_err)?
                .apply_keystream(&mut buf);
        }};
    }
    macro_rules! by_key {
        ($flavor:ident) => {
            match key.len() {
                16 => run!(ctr::$flavor<aes::Aes128>),
                24 => run!(ctr::$flavor<aes::Aes192>),
                32 => run!(ctr::$flavor<aes::Aes256>),
                _ => return Err(crypto_err("AES-CTR key must be 128, 192, or 256 bits")),
            }
        };
    }
    match counter_length {
        128 => by_key!(Ctr128BE),
        64 => by_key!(Ctr64BE),
        32 => by_key!(Ctr32BE),
        _ => return Err(crypto_err("AES-CTR supports counter lengths of 32, 64, or 128 bits")),
    }
    Ok(buf)
}

/// Generous upper bounds on PBKDF2 parameters. WebCrypto imposes no limit, but
/// page JS drives this op on the single-threaded runtime: an unbounded
/// iteration count pins the V8 isolate (blocking every other CDP command on the
/// connection) and a huge output length forces an unbounded `vec![0u8; length]`
/// allocation. Both caps sit far above any legitimate use — OWASP recommends
/// ~600k iterations and derived keys are tens of bytes.
const PBKDF2_MAX_ITERATIONS: u32 = 10_000_000;
const PBKDF2_MAX_OUTPUT_BYTES: u32 = 1024 * 1024;

/// PBKDF2 key derivation with DoS guards. Split out from the op so the bounds
/// are unit-testable without the `#[op2]` wrapper.
fn pbkdf2_derive(
    hash: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    length: u32,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    if iterations > PBKDF2_MAX_ITERATIONS {
        return Err(crypto_err(format!(
            "PBKDF2 iteration count {iterations} exceeds the supported maximum of {PBKDF2_MAX_ITERATIONS}"
        )));
    }
    if length > PBKDF2_MAX_OUTPUT_BYTES {
        return Err(crypto_err(format!(
            "PBKDF2 output length {length} bytes exceeds the supported maximum of {PBKDF2_MAX_OUTPUT_BYTES}"
        )));
    }
    use pbkdf2::pbkdf2_hmac;
    let mut dk = vec![0u8; length as usize];
    match hash {
        "SHA-1" => pbkdf2_hmac::<sha1::Sha1>(password, salt, iterations, &mut dk),
        "SHA-256" => pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut dk),
        "SHA-384" => pbkdf2_hmac::<sha2::Sha384>(password, salt, iterations, &mut dk),
        "SHA-512" => pbkdf2_hmac::<sha2::Sha512>(password, salt, iterations, &mut dk),
        _ => return Err(crypto_err("unsupported PBKDF2 hash")),
    }
    Ok(dk)
}

/// PBKDF2 key derivation. `length` is the derived-bits output in bytes.
#[op2]
#[buffer]
fn op_subtle_pbkdf2(
    #[string] hash: &str,
    #[buffer] password: &[u8],
    #[buffer] salt: &[u8],
    iterations: u32,
    length: u32,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    pbkdf2_derive(hash, password, salt, iterations, length)
}

/// HKDF key derivation. `length` is the output length in bytes. An empty salt
/// behaves as RFC 5869 specifies (HMAC zero-pads it to the block size, which is
/// what browsers do).
#[op2]
#[buffer]
fn op_subtle_hkdf(
    #[string] hash: &str,
    #[buffer] ikm: &[u8],
    #[buffer] salt: &[u8],
    #[buffer] info: &[u8],
    length: u32,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use hkdf::Hkdf;
    // Cap before allocating (obscura #910 family): RFC 5869 bounds output at
    // 255*HashLen anyway, but the old shape allocated `length` bytes BEFORE
    // expand() rejected an oversized request — a page-controlled length was
    // still a transient multi-hundred-MB spike even when it eventually
    // errored. Same treatment PBKDF2 gets via PBKDF2_MAX_OUTPUT_BYTES.
    if length > 65536 {
        return Err(crypto_err(format!(
            "HKDF output length {length} bytes exceeds the supported maximum of 65536"
        )));
    }
    let mut okm = vec![0u8; length as usize];
    macro_rules! run {
        ($d:ty) => {
            Hkdf::<$d>::new(Some(salt), ikm)
                .expand(info, &mut okm)
                .map_err(|_| crypto_err("HKDF: requested key length is too long"))?
        };
    }
    match hash {
        "SHA-1" => run!(sha1::Sha1),
        "SHA-256" => run!(sha2::Sha256),
        "SHA-384" => run!(sha2::Sha384),
        "SHA-512" => run!(sha2::Sha512),
        _ => return Err(crypto_err("unsupported HKDF hash")),
    }
    Ok(okm)
}

/// Fill `len` bytes from the OS CSPRNG. Backs `crypto.getRandomValues`,
/// `crypto.randomUUID`, and `generateKey`, replacing the old Math.random shim
/// (which was neither uniform across typed-array widths nor cryptographically
/// random, and was a fingerprinting tell).
#[op2]
#[buffer]
fn op_random_bytes(len: u32) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    // Output-length cap (obscura #910 family): the HMAC generateKey path
    // forwards a page-chosen key length here unclamped, and `vec![0u8; len]`
    // with a multi-hundred-MB len is an instant OOM abort rather than a
    // catchable error. 65536 matches the bound getRandomValues already
    // enforces per spec; no real key or UUID comes near it.
    if len > 65536 {
        return Err(crypto_err("requested random length exceeds 65536 bytes"));
    }
    let mut buf = vec![0u8; len as usize];
    getrandom::getrandom(&mut buf).map_err(|e| crypto_err(format!("getrandom failed: {e}")))?;
    Ok(buf)
}


/// Serialize a parsed URL into the WHATWG IDL component shape consumed by the
/// `URL` class in bootstrap.js. Getters read these fields directly so no op
/// call happens per property access.
fn url_components(u: &url::Url) -> serde_json::Value {
    let port = u.port().map(|p| p.to_string()).unwrap_or_default();
    let hostname = u.host_str().unwrap_or("").to_string();
    let host = if hostname.is_empty() {
        String::new()
    } else if port.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port}")
    };
    // WHATWG search/hash getters return "" for a null OR empty component.
    let search = match u.query() {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    let hash = match u.fragment() {
        Some(f) if !f.is_empty() => format!("#{f}"),
        _ => String::new(),
    };
    serde_json::json!({
        "ok": true,
        "href": u.as_str(),
        "protocol": format!("{}:", u.scheme()),
        "username": u.username(),
        "password": u.password().unwrap_or(""),
        "host": host,
        "hostname": hostname,
        "port": port,
        "pathname": u.path(),
        "search": search,
        "hash": hash,
        "origin": u.origin().ascii_serialization(),
    })
}

/// Parse `href` (optionally resolved against `base`) with the WHATWG-compliant
/// `url` crate. Returns the component JSON, or `{"ok":false}` when the input is
/// not a valid URL (the JS side turns that into a TypeError, per spec).
#[op2]
#[string]
fn op_url_parse(#[string] href: &str, #[string] base: &str) -> String {
    // The url crate can panic on a few pathological inputs (internal range
    // slicing); catch it so a bad URL never aborts the process.
    std::panic::catch_unwind(|| {
        let parsed = if base.is_empty() {
            url::Url::parse(href)
        } else {
            url::Url::parse(base).and_then(|b| b.join(href))
        };
        match parsed {
            Ok(u) => url_components(&u).to_string(),
            Err(_) => "{\"ok\":false}".to_string(),
        }
    })
    .unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

/// Apply a WHATWG URL setter (`part` = href/protocol/username/password/host/
/// hostname/port/pathname/search/hash) to `href` and return the new components.
fn url_set_inner(href: &str, part: &str, value: &str) -> Option<serde_json::Value> {
    let mut u = url::Url::parse(href).ok()?;
    match part {
        "href" => {
            let nu = url::Url::parse(value).ok()?;
            return Some(url_components(&nu));
        }
        "protocol" => {
            let _ = u.set_scheme(value.trim_end_matches(':'));
        }
        "username" => {
            let _ = u.set_username(value);
        }
        "password" => {
            let _ = u.set_password(if value.is_empty() { None } else { Some(value) });
        }
        "host" => set_host_port(&mut u, value),
        "hostname" => {
            if !value.is_empty() {
                let _ = u.set_host(Some(value));
            }
        }
        "port" => {
            // WHATWG port state consumes the leading ASCII digits and stops
            // at the first non-digit, so "8080abc" sets 8080; no digits or a
            // number above 65535 leaves the port untouched.
            if value.is_empty() {
                let _ = u.set_port(None);
            } else {
                let digits: String =
                    value.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse::<u32>() {
                    if p <= 65535 {
                        let _ = u.set_port(Some(p as u16));
                    }
                }
            }
        }
        "pathname" => u.set_path(value),
        "search" => {
            // Null only when the ORIGINAL value is the empty string; a bare
            // "?" sets an empty query that still serializes its delimiter
            // (href ends in "?").
            let q = value.strip_prefix('?').unwrap_or(value);
            u.set_query(if value.is_empty() { None } else { Some(q) });
        }
        "hash" => {
            let f = value.strip_prefix('#').unwrap_or(value);
            u.set_fragment(if value.is_empty() { None } else { Some(f) });
        }
        _ => {}
    }
    Some(url_components(&u))
}

#[op2]
#[string]
fn op_url_set(#[string] href: &str, #[string] part: &str, #[string] value: &str) -> String {
    // Some url-crate setters panic on pathological inputs (the url-setters WPT
    // tests exercise these). Catch the unwind and treat it as a no-op setter,
    // returning the URL unchanged, which matches WHATWG "do nothing on invalid".
    match std::panic::catch_unwind(|| url_set_inner(href, part, value)) {
        Ok(Some(v)) => v.to_string(),
        _ => match url::Url::parse(href) {
            Ok(u) => url_components(&u).to_string(),
            Err(_) => "{\"ok\":false}".to_string(),
        },
    }
}

/// Best-effort `host` setter: split `host[:port]` (handling bracketed IPv6) and
/// apply hostname and port separately, since `url::Url::set_host` rejects a port.
fn set_host_port(u: &mut url::Url, value: &str) {
    // IPv6 literals are bracketed; never split inside the brackets.
    if value.starts_with('[') {
        if let Some(close) = value.find(']') {
            let host = &value[..=close];
            let rest = &value[close + 1..];
            if u.set_host(Some(host)).is_ok() {
                if let Some(p) = rest.strip_prefix(':') {
                    if let Ok(pn) = p.parse::<u16>() {
                        let _ = u.set_port(Some(pn));
                    }
                }
            }
            return;
        }
    }
    if let Some(idx) = value.rfind(':') {
        let (h, p) = (&value[..idx], &value[idx + 1..]);
        if p.is_empty() || p.chars().all(|c| c.is_ascii_digit()) {
            if u.set_host(Some(h)).is_ok() {
                if p.is_empty() {
                    let _ = u.set_port(None);
                } else if let Ok(pn) = p.parse::<u16>() {
                    let _ = u.set_port(Some(pn));
                }
            }
            return;
        }
    }
    let _ = u.set_host(Some(value));
}

/// Resolve `href` against optional `base` and return only the serialized
/// absolute URL (no component breakdown). Used by the hot `a.href`/`area.href`
/// getter, which only needs the resolved string, so it avoids building and
/// re-parsing the full component JSON. Returns "" when the input is invalid.
#[op2]
#[string]
fn op_url_resolve(#[string] href: &str, #[string] base: &str) -> String {
    std::panic::catch_unwind(|| {
        let parsed = if base.is_empty() {
            url::Url::parse(href)
        } else {
            url::Url::parse(base).and_then(|b| b.join(href))
        };
        parsed.map(|u| u.as_str().to_string()).unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Parse and merge an inline document import map (upstream 34373c3). Returns
/// "" on success or the parse/merge error message, so the bootstrap caller can
/// surface it as a script error event without a rejected-op round-trip.
#[op2]
#[string]
fn op_add_import_map(
    state: &OpState,
    #[string] source: String,
    #[string] base_url: String,
) -> String {
    let shared = state.borrow::<SharedState>().clone();
    let import_map = shared.borrow().import_map.clone();
    let parsed = match crate::diting_js::import_map::ImportMap::parse(&source, &base_url) {
        Ok(map) => map,
        Err(error) => return error,
    };
    let result = match import_map.try_borrow_mut() {
        Ok(mut current) => {
            current.merge(parsed);
            String::new()
        }
        Err(_) => "Import map is already borrowed".to_string(),
    };
    result
}

/// Canonical (lowercased) WHATWG name for a TextDecoder label, or "" if the
/// label is unknown (the JS constructor turns "" into a RangeError).
#[op2]
#[string]
fn op_encoding_for_label(#[string] label: &str) -> String {
    crate::diting_net::label_name(label).unwrap_or_default()
}

/// Decode bytes with a legacy/explicit encoding via encoding_rs. Returns
/// {"ok":true,"v":<string>} or {"ok":false} (unknown label, or a fatal decode
/// error). The UTF-8 non-fatal common case is handled in JS without this op.
#[op2]
#[string]
fn op_text_decode(#[string] label: &str, #[buffer] bytes: &[u8], fatal: bool, ignore_bom: bool) -> String {
    match crate::diting_net::decode_with_label(label, bytes, fatal, ignore_bom) {
        Some(s) => serde_json::json!({ "ok": true, "v": s }).to_string(),
        None => "{\"ok\":false}".to_string(),
    }
}

/// Re-encode a URL query component using a non-UTF-8 document encoding override
/// (the WHATWG "encoding override"). `query` is the already-UTF-8-decoded query
/// string; `label` the target charset; `special` whether the URL has a special
/// scheme (adds `'` to the percent-encode set). Returns the encoded query, or
/// the input unchanged if the label is unknown. Only called by the JS anchor
/// path when the document is non-UTF-8, so the UTF-8 hot path never reaches it.
#[op2]
#[string]
fn op_url_encode_query(#[string] query: &str, #[string] label: &str, special: bool) -> String {
    crate::diting_net::url_encode_query(query, label, special).unwrap_or_else(|| query.to_string())
}

pub fn build_extension() -> Extension {
    Extension {
        name: "diting_dom",
        ops: std::borrow::Cow::Owned(vec![
            op_dom(),
            op_shadow_attach(),
            op_console_msg(),
            op_dialog(),
            op_script_mark_started(),
            op_script_try_start(),
            op_dyn_script_fetch_begin(),
            op_dyn_script_fetch_end(),
            op_blob_register(),
            op_blob_revoke(),
            op_fetch_url(),
            op_fetch_url_sync(),
            op_get_cookies(),
            op_set_cookie(),
            op_storage_read(),
            op_storage_write(),
            op_navigate(),
            op_sleep(),
            op_binding_called(),
            op_subtle_digest(),
            op_subtle_hmac(),
            op_subtle_aes_gcm(),
            op_subtle_aes_cbc(),
            op_subtle_aes_ctr(),
            op_subtle_pbkdf2(),
            op_subtle_hkdf(),
            op_random_bytes(),
            op_url_parse(),
            op_url_set(),
            op_url_resolve(),
            op_add_import_map(),
            op_encoding_for_label(),
            op_text_decode(),
            op_url_encode_query(),
            crate::diting_js::ws::op_ws_open(),
            crate::diting_js::ws::op_ws_next_message(),
            crate::diting_js::ws::op_ws_send(),
            crate::diting_js::ws::op_ws_close(),
        ]),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cors_response_allows, cors_unsafe_request_header_names, is_cors_safelisted_content_type,
        is_cors_safelisted_request_header, parse_cors_header_list, preflight_allows_header,
        preflight_allows_method, validate_fetch_url, FetchCredentials,
    };
    use super::{pbkdf2_derive, PBKDF2_MAX_ITERATIONS, PBKDF2_MAX_OUTPUT_BYTES};

    /// The #395 paint-only predicate: only a name diff inside
    /// {transform, opacity} may keep the solve cache. Value changes,
    /// whitelist additions/removals, prefixed properties and missing
    /// before-states must all fall back to the full invalidation.
    /// Chrome's computed backdrop-filter face: "blur(<length>)" or "none".
    /// The props-table membership is asserted here too — a serialization
    /// arm without a table entry is invisible to getComputedStyle.
    #[cfg(feature = "screenshot")]
    #[test]
    fn backdrop_filter_computed_face() {
        assert!(super::COMPUTED_STYLE_PROPS.contains(&"backdrop-filter"));
        let mut s = crate::diting_css::ComputedStyle::default();
        assert_eq!(
            super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
            Some("none")
        );
        s.backdrop_blur = Some(12.0);
        assert_eq!(
            super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
            Some("blur(12px)")
        );
        s.backdrop_blur = Some(2.5);
        assert_eq!(
            super::computed_style_value(&s, "backdrop-filter", None).as_deref(),
            Some("blur(2.5px)")
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn style_write_paint_only_diff_matrix() {
        use super::{style_property_names, style_write_is_paint_only};
        // Values differ freely while the names stay — the seek case.
        assert!(style_write_is_paint_only(
            Some("transform: translate3d(-200px, 0px, 0px); opacity: 0"),
            Some("transform: translate3d(-32.4821px, 0px, 0px); opacity: 0.8376"),
        ));
        // Adding a whitelisted property to an existing whitelisted set.
        assert!(style_write_is_paint_only(
            Some("transform: translate3d(-200px, 0px, 0px)"),
            Some("transform: translate3d(-12px, 0px, 0px); opacity: 0.5"),
        ));
        // Clearing the animation off a style that only ever held it.
        assert!(style_write_is_paint_only(
            Some("opacity: 0.42; transform: scale(2)"),
            Some(""),
        ));
        // A geometry property appears — full invalidation.
        assert!(!style_write_is_paint_only(
            Some("transform: translate3d(-200px, 0px, 0px); opacity: 0"),
            Some("transform: translate3d(-200px, 0px, 0px); opacity: 0; width: 300px"),
        ));
        // Prefixed transform is a different name — never whitelisted on a
        // guess.
        assert!(!style_write_is_paint_only(
            Some("transform: scale(2)"),
            Some("transform: scale(2); -webkit-transform: scale(2)"),
        ));
        // A geometry property disappears.
        assert!(!style_write_is_paint_only(
            Some("width: 300px; opacity: 0.5"),
            Some("opacity: 0.5"),
        ));
        // No before-state to diff — conservative full drop (first write on
        // a bare element).
        assert!(!style_write_is_paint_only(None, Some("opacity: 0")));
        assert!(!style_write_is_paint_only(Some("opacity: 0"), None));
        // Name extraction: casing, whitespace and empty fragments.
        assert_eq!(
            style_property_names("TRANSFORM: scale(2) ; ; opacity:0"),
            ["transform", "opacity"].into_iter().map(str::to_string).collect::<std::collections::HashSet<_>>()
        );
    }

    // Ephemeral deployments must not persist login tokens: storage_file is
    // the single choke point every localStorage flush goes through.
    #[test]
    fn storage_file_is_none_under_ephemeral() {
        let _env = crate::config::EPHEMERAL_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("AGINXBROWSER_EPHEMERAL");
        assert!(super::storage_file("https://example.com").is_some());

        std::env::set_var("AGINXBROWSER_EPHEMERAL", "1");
        assert_eq!(super::storage_file("https://example.com"), None);
        assert_eq!(
            super::storage_file("https://login.taobao.com"),
            None,
            "the gate must sit above every origin"
        );
        std::env::remove_var("AGINXBROWSER_EPHEMERAL");
    }

    // obscura "enforce CORS preflight permissions" (04f0475) same-hole port:
    // the safelist decides whether a cross-origin request even needs a
    // preflight, so "any content-type" passing would have made every JSON
    // API call a simple request.
    #[test]
    fn cors_safelist_content_type_is_value_sensitive() {
        for ok in [
            "text/plain",
            "text/plain; charset=utf-8",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=----x",
            "TEXT/PLAIN",
        ] {
            assert!(is_cors_safelisted_content_type(ok), "must be safelisted: {ok}");
        }
        for blocked in [
            "application/json",
            "application/json; charset=utf-8",
            "text/html",
            "text/plain()",
            "nonsense",
            "text/plain; utf-8\u{7f}",
        ] {
            assert!(
                !is_cors_safelisted_content_type(blocked),
                "must NOT be safelisted: {blocked}"
            );
        }
    }

    #[test]
    fn cors_unsafe_header_names_sorted_and_lowercased() {
        let headers: std::collections::HashMap<String, String> = [
            ("Content-Type", "application/json".to_string()), // unsafe value
            ("X-Custom", "anything".to_string()),              // unsafe name
            ("Accept", "text/html".to_string()),               // safelisted
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        assert_eq!(
            cors_unsafe_request_header_names(&headers),
            vec!["content-type".to_string(), "x-custom".to_string()]
        );

        let simple: std::collections::HashMap<String, String> = [
            ("accept".to_string(), "text/html".to_string()),
            ("content-type".to_string(), "text/plain".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(cors_unsafe_request_header_names(&simple).is_empty());
    }

    // The preflight gate itself: "*" is consent for anonymous requests only,
    // and never for the Authorization header (Fetch standard).
    #[test]
    fn preflight_permissions_respect_credentials_and_authorization() {
        let put: reqwest::Method = reqwest::Method::PUT;
        let get = reqwest::Method::GET;

        assert!(preflight_allows_method(&put, &["PUT", "POST"], true));
        assert!(preflight_allows_method(&get, &[], true)); // safelisted method
        assert!(!preflight_allows_method(&put, &["POST"], false));
        assert!(preflight_allows_method(&put, &["*"], false));
        assert!(!preflight_allows_method(&put, &["*"], true));

        assert!(preflight_allows_header("content-type", &["Content-Type"], false));
        assert!(preflight_allows_header("x-custom", &["*"], false));
        assert!(!preflight_allows_header("x-custom", &["*"], true));
        assert!(!preflight_allows_header("authorization", &["*"], false));
    }

    #[test]
    fn parse_cors_header_list_rejects_non_tokens() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "access-control-allow-methods",
            "GET, PUT".parse().unwrap(),
        );
        assert_eq!(
            parse_cors_header_list(&headers, "access-control-allow-methods"),
            Some(vec!["GET", "PUT"])
        );

        // Absent header parses to empty (a server that said nothing allowed
        // nothing) — but a malformed value is a parse failure.
        assert_eq!(
            parse_cors_header_list(&headers, "access-control-allow-headers"),
            Some(Vec::new())
        );
        headers.insert(
            "access-control-allow-headers",
            "X-Custom, bad name".parse().unwrap(),
        );
        assert_eq!(
            parse_cors_header_list(&headers, "access-control-allow-headers"),
            None
        );
    }

    // Range is safelisted only as a valid byte range (Fetch standard): the
    // suffix form and inverted ranges must not ride the safelist.
    #[test]
    fn cors_safelisted_range_requires_valid_byte_range() {
        assert!(is_cors_safelisted_request_header("range", "bytes=0-1023"));
        assert!(is_cors_safelisted_request_header("range", "bytes=0-"));
        assert!(!is_cors_safelisted_request_header("range", "bytes=1023-0"));
        assert!(!is_cors_safelisted_request_header("range", "bytes=-500"));
        assert!(!is_cors_safelisted_request_header("range", "items=0-10"));
    }

    // Upstream obscura #708: file:// must be rejected up front for
    // page-reachable fetch/XHR (deny-by-default, matching navigation),
    // not allowed through the scheme gate to short-circuit SSRF checks.
    #[test]
    fn fetch_scheme_gate_rejects_file_up_front() {
        let file_url = url::Url::parse("file:///etc/passwd").unwrap();
        let err = validate_fetch_url(&file_url).unwrap_err();
        assert!(err.contains("Forbidden URL scheme 'file'"), "got: {err}");

        let ftp = url::Url::parse("ftp://example.com/x").unwrap();
        assert!(validate_fetch_url(&ftp).is_err());

        let https = url::Url::parse("https://example.com/x").unwrap();
        assert!(validate_fetch_url(&https).is_ok());
    }

    // The fetch gate must share the navigation deny-set, not a local
    // re-listing: 198.18.0.0/15 (benchmarking), 100.64/10 (CGNAT metadata),
    // 0.0.0.0, IPv4-mapped loopback and 6to4-wrapped link-local were all
    // fetchable from page JS while the navigation gate blocked them
    // (obscura #852 family).
    #[test]
    fn fetch_gate_shares_navigation_deny_set() {
        let _lock = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        let prev = std::env::var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK").ok();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        for bad in [
            "http://198.18.0.1/",         // benchmarking range
            "http://100.100.100.200/",    // CGNAT cloud metadata
            "http://0.0.0.0/",            // unspecified, routes to localhost
            "http://[::ffff:127.0.0.1]/", // IPv4-mapped loopback
            "http://[2002:a9fe:a9fe::]/", // 6to4-wrapped link-local
        ] {
            let u = url::Url::parse(bad).unwrap();
            let err = validate_fetch_url(&u).unwrap_err();
            assert!(err.contains("not allowed"), "{bad}: got {err}");
        }

        assert!(validate_fetch_url(&url::Url::parse("http://example.com/x").unwrap()).is_ok());

        if let Some(v) = prev {
            std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", v);
        }
    }

    // Upstream b744b9b.
    #[test]
    fn fetch_credentials_gate_cookies_per_request_origin() {
        let page_origin = "https://www.example.com";
        let same = "https://www.example.com/api";
        let explicit_default_port = "https://www.example.com:443/api";
        let cross = "https://api.example.com/data";

        assert!(!FetchCredentials::Omit.allows(page_origin, same));
        assert!(!FetchCredentials::Omit.allows(page_origin, cross));

        assert!(FetchCredentials::SameOrigin.allows(page_origin, same));
        assert!(FetchCredentials::SameOrigin.allows(page_origin, explicit_default_port));
        assert!(!FetchCredentials::SameOrigin.allows(page_origin, cross));

        assert!(FetchCredentials::Include.allows(page_origin, same));
        assert!(FetchCredentials::Include.allows(page_origin, cross));
    }

    // Upstream b744b9b.
    #[test]
    fn credentialed_cors_requires_exact_origin_and_allow_credentials() {
        let page_origin = "https://www.example.com";

        assert!(cors_response_allows(FetchCredentials::SameOrigin, page_origin, "*", ""));
        assert!(cors_response_allows(FetchCredentials::SameOrigin, page_origin, page_origin, ""));
        assert!(!cors_response_allows(FetchCredentials::SameOrigin, page_origin, "https://other.example", ""));

        assert!(cors_response_allows(FetchCredentials::Include, page_origin, page_origin, "true"));
        assert!(!cors_response_allows(FetchCredentials::Include, page_origin, "*", ""));
        assert!(!cors_response_allows(FetchCredentials::Include, page_origin, page_origin, ""));
        assert!(!cors_response_allows(FetchCredentials::Include, page_origin, "https://other.example", "true"));
    }

    // Upstream cfda91b / #580 — PBKDF2 parameters arrive straight from page JS.
    // Without caps, a huge iteration count pins the single-threaded runtime and
    // a huge output length forces an unbounded allocation.
    #[test]
    fn pbkdf2_rejects_excessive_iterations() {
        let err = pbkdf2_derive("SHA-256", b"pw", b"salt", PBKDF2_MAX_ITERATIONS + 1, 32)
            .expect_err("iteration count above the cap must be rejected");
        assert!(
            err.to_string().contains("iteration"),
            "error should name the iteration cap: {err}"
        );
    }

    #[test]
    fn pbkdf2_rejects_excessive_output_length() {
        let err = pbkdf2_derive("SHA-256", b"pw", b"salt", 1_000, PBKDF2_MAX_OUTPUT_BYTES + 1)
            .expect_err("output length above the cap must be rejected");
        assert!(
            err.to_string().contains("length"),
            "error should name the length cap: {err}"
        );
    }

    #[test]
    fn pbkdf2_derives_within_limits() {
        let dk = pbkdf2_derive("SHA-256", b"password", b"salt", 1_000, 32)
            .expect("ordinary parameters must derive successfully");
        assert_eq!(dk.len(), 32, "derived key must have the requested length");
    }
}
