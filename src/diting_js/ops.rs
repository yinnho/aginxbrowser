use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use deno_core::op2;
use deno_core::OpState;
use deno_core::Extension;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crate::diting_dom::{DomTree, NodeData, NodeId};
use html5ever::namespace_url;
use crate::diting_net::{CookieJar, HttpClient};

/// CDP Fetch-domain resolution: what a client answers a paused request with.
/// Only `Continue` is ever produced today (by tests); `Fulfill` / `Fail` are
/// the wire shapes a CDP client would send — no CDP client exists yet.
#[allow(dead_code)]
#[derive(Debug)]
pub enum InterceptResolution {
    Continue {
        url: Option<String>,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        body: Option<String>,
    },
    Fulfill {
        status: u16,
        headers: HashMap<String, String>,
        body: String,
    },
    Fail { reason: String },
}

/// A paused request surfaced to the interception channel (CDP
/// `Fetch.requestPaused` shape). The resolver answers with an
/// `InterceptResolution`. Field readers are the CDP layer, which is not
/// absorbed; ops code only moves the struct through the channel.
#[allow(dead_code)]
pub struct InterceptedRequest {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub resource_type: String,
    pub resolver: tokio::sync::oneshot::Sender<InterceptResolution>,
}

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
    pub blocked_urls: Vec<String>,
    pub cookie_jar: Option<Arc<CookieJar>>,
    pub http_client: Option<Arc<HttpClient>>,
    pub pending_navigation: Option<(String, String, String)>,
    pub intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    pub intercept_counter: u64,
    pub intercept_enabled: bool,
    // Queue of (binding_name, payload) calls made by page JS via the
    // `op_binding_called` op. Drained by the CDP layer after each dispatch
    // and emitted as `Runtime.bindingCalled` events.
    pub pending_binding_calls: Vec<(String, String)>,
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
    /// Memoized diting-layout rects for the live DOM tree, keyed by
    /// the tree's epoch (see DomTree::epoch). Filled on the first
    /// `layout_rect` op after each mutation; backs getBoundingClientRect.
    layout_cache: std::cell::RefCell<Option<(u64, HashMap<NodeId, [f32; 4]>)>>,
    /// Viewport the layout pipeline should anchor the initial containing
    /// block to, published by the JS persona (`__diting_setPersona`) so
    /// getBoundingClientRect agrees with window.innerWidth/innerHeight.
    /// Defaults to the bootstrap's pre-persona 1920x1000.
    #[cfg(feature = "screenshot")]
    pub(crate) viewport: (f32, f32),
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
}

/// A response body retained for `Network.getResponseBody`. Text bodies are
/// stored lossy-UTF-8 (`base64_encoded = false`); binary bodies base64.
#[cfg_attr(not(test), allow(dead_code))] // tests read both fields; CDP consumer pending
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

/// True when a Content-Type deserves text storage rather than base64. Mirrors
/// the page-side document/script/stylesheet decision (no Content-Type at all
/// counts as text, matching the HTML-parse default).
fn text_like_content_type(content_type: Option<&str>) -> bool {
    let ct = match content_type {
        Some(c) => c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase(),
        None => return true,
    };
    if ct.is_empty() {
        return true;
    }
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/xhtml+xml"
        || ct == "application/javascript"
        || ct == "application/ecmascript"
        || ct == "image/svg+xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

impl JsState {
    pub fn new() -> Self {
        JsState {
            dom: None,
            url: "about:blank".to_string(),
            encoding: "UTF-8".to_string(),
            title: String::new(),
            referrer: String::new(),
            blocked_urls: Vec::new(),
            cookie_jar: None,
            http_client: None,
            pending_navigation: None,
            intercept_tx: None,
            intercept_counter: 0,
            intercept_enabled: false,
            pending_binding_calls: Vec::new(),
            write_stream: std::cell::RefCell::new(None),
            already_started_scripts: RefCell::new(HashSet::new()),
            import_map: Rc::new(RefCell::new(crate::diting_js::import_map::ImportMap::default())),
            dynamic_script_fetches: std::cell::Cell::new(0),
            callbacks: None,
            network_response_bodies: std::collections::HashMap::new(),
            network_response_body_order: std::collections::VecDeque::new(),
            network_response_body_counter: 0,
            js_network_events: Vec::new(),
            // Memoized diting-layout rects for the live DOM tree, keyed by
            // the tree's epoch (see DomTree::epoch). Filled on the first
            // `layout_rect` op after each mutation; backs getBoundingClientRect.
            layout_cache: std::cell::RefCell::new(None),
            #[cfg(feature = "screenshot")]
            viewport: (1920.0, 1000.0),
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
            *gs.layout_cache.borrow_mut() = None;
        }
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
                NodeData::Document => "#document".to_string(), NodeData::Element { name, .. } => name.local.as_ref().to_ascii_uppercase(),
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
        "child_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom.children(NodeId::new(nid)).iter().map(|id| id.index() as i32).collect();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "tag_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name = dom.get_node(NodeId::new(nid)).and_then(|n| n.as_element().map(|name| name.local.as_ref().to_ascii_uppercase())).unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
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
            dom.new_node(NodeData::Element {
                name: html5ever::QualName::new(None, html5ever::ns!(html), html5ever::LocalName::from(arg1.as_str())),
                attrs: vec![], template_contents: None, mathml_annotation_xml_integration_point: false,
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
            let cache_hit = gs.layout_cache.borrow().as_ref().and_then(|(e, m)| {
                if *e == epoch { m.get(&nid).copied() } else { None }
            });
            let rect = match cache_hit {
                Some(r) => Some(r),
                None => {
                    let rects = layout_rects_all(&gs, dom);
                    let r = rects.get(&nid).copied();
                    *gs.layout_cache.borrow_mut() = Some((epoch, rects));
                    r
                }
            };
            match rect {
                Some([x, y, w, h]) => format!("[{},{},{},{}]", x, y, w, h),
                None => "null".into(),
            }
        }
        #[cfg(not(feature = "screenshot"))]
        "layout_rect" => "null".into(),
        _ => "null".into(),
    }
}

/// Run the full diting style + layout pipeline over the live DOM tree and
/// return every element's border-box rect. Styles are re-collected each run:
/// attribute-level mutations (style/class writes) don't bump the tree epoch,
/// so memoizing computed styles alongside the rects would serve stale geometry.
#[cfg(feature = "screenshot")]
fn layout_rects_all(gs: &JsState, dom: &DomTree) -> HashMap<NodeId, [f32; 4]> {
    // Same viewport the persona publishes to window.innerWidth/innerHeight,
    // so geometry agrees with what scripts read off `window` (and the ICB
    // has a definite size for fixed-box inset resolution — obscura#675).
    let (viewport_width, viewport_height) = gs.viewport;
    let mut css = String::new();
    if let Ok(style_els) = dom.query_selector_all("style") {
        for el in style_els {
            css.push_str(&dom.text_content(el));
            css.push('\n');
        }
    }
    let rules = crate::diting_css::parse_stylesheet_for(
        &css,
        (viewport_width, viewport_height),
        crate::diting_css::CssMediaType::Screen,
    );
    let styles = crate::diting_layout::compute_styles(dom, &rules);
    let fonts = crate::diting_fonts::font_book();
    crate::diting_layout::layout_dom(dom, &styles, &fonts, viewport_width, viewport_height)
        .into_iter()
        .map(|(id, r)| (id, [r.x, r.y, r.width, r.height]))
        .collect()
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
    let _ = state;
    match level {
        "warn" => tracing::warn!(target: "diting::console", "{}", msg),
        "error" => tracing::error!(target: "diting::console", "{}", msg),
        _ => tracing::info!(target: "diting::console", "{}", msg),
    }
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
    let mut builder = reqwest::Client::builder()
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

#[op2(async)]
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
) -> Result<String, deno_error::JsErrorBox> {
    tracing::debug!("op_fetch_url called: {} {} (intercept check pending)", method, url);

    if let Ok(parsed_url) = url::Url::parse(&url) {
        if let Err(e) = validate_fetch_url(&parsed_url) {
            return Ok(serde_json::json!({
                "status": 0,
                "body": "",
                "url": url,
                "headers": {},
                "blocked": true,
                "error": e,
            }).to_string());
        }
    }

    let (cookie_jar, in_flight, intercept_tx, proxy_url, http_client, callbacks) = {
        let state_borrow = state.borrow();
        let gs = state_borrow.borrow::<SharedState>().clone();
        let mut gs = gs.borrow_mut();
        for pattern in &gs.blocked_urls {
            if pattern == "*" || url.contains(pattern) || glob_match(pattern, &url) {
                return Ok(serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": url,
                    "headers": {},
                    "blocked": true,
                }).to_string());
            }
        }
        let jar = gs.cookie_jar.clone();
        let in_flight = gs.http_client.as_ref().map(|c| c.in_flight.clone());
        // #139: thread the configured proxy through to the per-request
        // reqwest::Client. Without this, op_fetch_url silently bypasses
        // BrowserContext.proxy_url for every JS fetch() / XHR call.
        let proxy_url = gs.http_client.as_ref().and_then(|c| c.proxy_url().map(|s| s.to_string()));
        tracing::debug!("op_fetch_url: intercept_enabled={}, has_tx={}", gs.intercept_enabled, gs.intercept_tx.is_some());
        let itx = if gs.intercept_enabled {
            gs.intercept_counter += 1;
            gs.intercept_tx.clone().map(|tx| (tx, format!("intercept-{}", gs.intercept_counter)))
        } else {
            None
        };
        (jar, in_flight, itx, proxy_url, gs.http_client.clone(), gs.callbacks.clone())
    };

    let mut override_url: Option<String> = None;
    let mut override_method: Option<String> = None;
    let mut override_headers: Option<HashMap<String, String>> = None;
    let mut override_body: Option<String> = None;
    if let Some((tx, request_id)) = intercept_tx {
        let custom_headers: HashMap<String, String> = serde_json::from_str(&headers_json).unwrap_or_default();
        let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();
        let intercepted = InterceptedRequest {
            request_id: request_id.clone(),
            url: url.clone(),
            method: method.clone(),
            headers: custom_headers.clone(),
            resource_type: "Fetch".to_string(),
            resolver: resolve_tx,
        };
        if tx.send(intercepted).is_ok() {
            match resolve_rx.await {
                Ok(InterceptResolution::Fulfill { status, headers: h, body: b }) => {
                    let resp_headers: HashMap<String, String> = h;
                    return Ok(serde_json::json!({
                        "status": status,
                        "body": b,
                        "url": url,
                        "headers": resp_headers,
                    }).to_string());
                }
                Ok(InterceptResolution::Fail { reason }) => {
                    return Ok(serde_json::json!({
                        "status": 0,
                        "body": "",
                        "url": url,
                        "headers": {},
                        "blocked": true,
                        "error": reason,
                    }).to_string());
                }
                Ok(InterceptResolution::Continue { url: new_url, method: new_method, headers: new_headers, body: new_body }) => {
                    override_url = new_url;
                    override_method = new_method;
                    override_headers = new_headers;
                    override_body = new_body;
                }
                Err(_) => {
                }
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
                return Ok(serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": new_url,
                    "blocked": true,
                    "error": format!("Intercept rewrite to forbidden URL blocked: {}", reason),
                }).to_string());
            }
        }
        new_url
    } else {
        url
    };
    let method = override_method.unwrap_or(method);
    let body = override_body.unwrap_or(body);
    let headers_json = match override_headers {
        Some(h) => serde_json::to_string(&h).unwrap_or(headers_json),
        None => headers_json,
    };

    // Pages use their context-scoped client so sequential runtimes never
    // share an async connection pool (upstream ab6fa0e, #453). The
    // process-wide cache remains the fallback for runtimes with no owning
    // HttpClient (e.g. a bare module-loader runtime).
    let client = match &http_client {
        Some(client) => client.request_client().await,
        None => select_request_client(&url, proxy_url.as_deref())
            .await
            .map_err(deno_error::JsErrorBox::generic)?,
    };

    // url::Url::origin() normalizes default ports, so an explicit :443 still
    // compares same-origin (the old hand-rolled form did not).
    let initial_request_origin = request_origin(&url).unwrap_or_default();
    let page_origin = if origin.is_empty() { initial_request_origin.clone() } else { origin.clone() };
    let is_cross_origin = !page_origin.is_empty() && initial_request_origin != page_origin;
    let credentials = FetchCredentials::parse(&credentials);

    let req_method: reqwest::Method = method.parse().unwrap_or(reqwest::Method::GET);

    let custom_headers: std::collections::HashMap<String, String> =
        serde_json::from_str(&headers_json).unwrap_or_default();

    let needs_preflight = is_cross_origin
        && mode == "cors"
        && (req_method != reqwest::Method::GET
            && req_method != reqwest::Method::HEAD
            && req_method != reqwest::Method::POST
            || custom_headers.keys().any(|k| {
                let kl = k.to_lowercase();
                kl != "accept" && kl != "accept-language" && kl != "content-language"
                    && kl != "content-type"
            }));

    if needs_preflight {
        let preflight = client
            .request(reqwest::Method::OPTIONS, &url)
            .header("Origin", &page_origin)
            .header("Access-Control-Request-Method", method.as_str())
            .header(
                "Access-Control-Request-Headers",
                custom_headers.keys().cloned().collect::<Vec<_>>().join(", "),
            )
            .send()
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(format!("CORS preflight failed: {}", e)))?;

        let allowed_origin = preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let allow_credentials = preflight
            .headers()
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !cors_response_allows(credentials, &page_origin, allowed_origin, allow_credentials) {
            return Err(deno_error::JsErrorBox::generic(format!(
                "CORS preflight: Origin '{}' not allowed by Access-Control-Allow-Origin '{}'",
                page_origin, allowed_origin
            )));
        }
    }

    // Follow redirects manually so the SSRF policy applies to every hop.
    // reqwest's auto-follow would bypass validate_fetch_url on the redirect
    // target and let an attacker-allowed origin 302 to http://127.0.0.1
    // (GHSA-8v6v-g4rh-jmcm).
    let mut current_url = url.clone();
    let mut current_method = req_method;
    let mut current_body = body;
    let mut redirects_followed: usize = 0;

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

    let response = loop {
        let mut req = client.request(current_method.clone(), &current_url);

        // Cross-origin and credentials are per-hop: a redirect can change
        // either answer (upstream b744b9b).
        let current_is_cross_origin = request_origin(&current_url)
            .map(|o| o != page_origin)
            .unwrap_or(false);
        if current_is_cross_origin {
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

        // Send a default User-Agent on fetch()/XHR requests (the navigation path
        // sets one, but this op did not, so scripted requests went out with no UA
        // and UA-gated servers rejected them). Honor an explicit override.
        if !custom_headers.keys().any(|k| k.eq_ignore_ascii_case("user-agent")) {
            req = req.header(
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
            );
        }

        for (k, v) in &custom_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        if !current_body.is_empty() {
            req = req.body(current_body.clone());
        }

        if let Some(ref counter) = in_flight {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let resp = req.send().await.map_err(|e| {
            if let Some(ref counter) = in_flight {
                counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            deno_error::JsErrorBox::generic(e.to_string())
        })?;

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
            break resp;
        }

        let location_header = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location_header else {
            // 3xx without a Location header is not actually a redirect.
            break resp;
        };

        let base = match url::Url::parse(&current_url) {
            Ok(b) => b,
            Err(_) => break resp,
        };
        let next_url = match base.join(&location) {
            Ok(u) => u,
            Err(_) => break resp,
        };

        // Re-validate every redirect target against the SSRF policy.
        if let Err(reason) = validate_fetch_url(&next_url) {
            return Ok(serde_json::json!({
                "status": 0,
                "body": "",
                "url": next_url.to_string(),
                "headers": {},
                "blocked": true,
                "error": format!("Redirect to forbidden URL blocked: {}", reason),
            })
            .to_string());
        }

        redirects_followed += 1;
        if redirects_followed > FETCH_REDIRECT_LIMIT {
            return Ok(serde_json::json!({
                "status": 0,
                "body": "",
                "url": next_url.to_string(),
                "headers": {},
                "blocked": true,
                "error": format!("Too many redirects (>{})", FETCH_REDIRECT_LIMIT),
            })
            .to_string());
        }

        // Browser semantics: 301/302/303 downgrade to GET with no body.
        // 307/308 preserve method and body.
        let status_code = resp.status().as_u16();
        if status_code == 301 || status_code == 302 || status_code == 303 {
            current_method = reqwest::Method::GET;
            current_body.clear();
        }

        current_url = next_url.to_string();
    };

    let status = response.status().as_u16();

    let resp_headers: std::collections::HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

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
            return Ok(serde_json::json!({
                "status": 0,
                "body": "",
                "url": url,
                "headers": {},
                "corsBlocked": true,
                "corsError": if credentials == FetchCredentials::Include {
                    format!("CORS error: credentialed request requires Access-Control-Allow-Origin '{}' and Access-Control-Allow-Credentials 'true'", page_origin)
                } else {
                    format!("CORS error: Origin '{}' not in Access-Control-Allow-Origin '{}'", page_origin, allowed)
                },
            })
            .to_string());
        }
    }

    let resp_bytes = response
        .bytes()
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    let resp_body = String::from_utf8_lossy(&resp_bytes).to_string();
    let resp_body_base64 = BASE64.encode(&resp_bytes);

    // Retain the body + record a network event for this script-initiated
    // request (upstream #406/#360): keyed `fetch-{N}`, LRU-bounded, so the
    // CDP layer can emit Network events and resolve getResponseBody for
    // fetch()/XHR traffic. Then fire the passive on_response observers.
    let request_id = {
        let state_borrow = state.borrow();
        let gs = state_borrow.borrow::<SharedState>().clone();
        let mut gs = gs.borrow_mut();
        gs.network_response_body_counter += 1;
        let request_id = format!("fetch-{}", gs.network_response_body_counter);
        let max_entries = response_body_entry_limit();
        let max_bytes = response_body_byte_limit();
        let base64_encoded =
            !text_like_content_type(resp_headers.get("content-type").map(|s| s.as_str()));
        if max_entries > 0 && max_bytes > 0 && resp_bytes.len() <= max_bytes {
            gs.network_response_bodies.insert(
                request_id.clone(),
                StoredNetworkResponseBody {
                    body: if base64_encoded {
                        resp_body_base64.clone()
                    } else {
                        resp_body.clone()
                    },
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
            url: url.clone(),
            method: method.clone(),
            status,
            response_headers: resp_headers.clone(),
            body_size: resp_bytes.len(),
            timestamp,
        });
        const MAX_JS_NETWORK_EVENTS: usize = 4096;
        if gs.js_network_events.len() > MAX_JS_NETWORK_EVENTS {
            let overflow = gs.js_network_events.len() - MAX_JS_NETWORK_EVENTS;
            gs.js_network_events.drain(0..overflow);
        }
        request_id
    };
    tracing::debug!(
        "op_fetch_url completed: {} {} ({} bytes, network event {})",
        method,
        url,
        resp_body.len(),
        request_id,
    );

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

    Ok(serde_json::json!({
        "status": status,
        "body": resp_body,
        "bodyBase64": resp_body_base64,
        "url": url,
        "headers": resp_headers,
    })
    .to_string())
}

fn glob_match(pattern: &str, url: &str) -> bool {
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

fn validate_fetch_url(url: &url::Url) -> Result<(), String> {
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
                if ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_documentation()
                {
                    return Err(format!(
                        "Access to private/internal IP address {} is not allowed",
                        ip
                    ));
                }
            }
            url::Host::Ipv6(ip) => {
                if ip.is_loopback() || ip.is_unicast_link_local() {
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

#[op2(fast)]
fn op_navigate(state: &OpState, #[string] url: &str, #[string] method: &str, #[string] body: &str) {
    let gs = state.borrow::<SharedState>().clone();
    let mut gs = gs.borrow_mut();
    gs.url = url.to_string();
    gs.pending_navigation = Some((url.to_string(), method.to_string(), body.to_string()));
}

#[op2(async)]
async fn op_sleep(#[number] millis: u64) {
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
            if value.is_empty() {
                let _ = u.set_port(None);
            } else if let Ok(p) = value.parse::<u16>() {
                let _ = u.set_port(Some(p));
            }
        }
        "pathname" => u.set_path(value),
        "search" => {
            let q = value.strip_prefix('?').unwrap_or(value);
            u.set_query(if q.is_empty() { None } else { Some(q) });
        }
        "hash" => {
            let f = value.strip_prefix('#').unwrap_or(value);
            u.set_fragment(if f.is_empty() { None } else { Some(f) });
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
            op_console_msg(),
            op_script_mark_started(),
            op_script_try_start(),
            op_dyn_script_fetch_begin(),
            op_dyn_script_fetch_end(),
            op_fetch_url(),
            op_get_cookies(),
            op_set_cookie(),
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
        ]),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{cors_response_allows, validate_fetch_url, FetchCredentials};
    use super::{pbkdf2_derive, PBKDF2_MAX_ITERATIONS, PBKDF2_MAX_OUTPUT_BYTES};

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
