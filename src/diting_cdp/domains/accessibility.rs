//! CDP Accessibility domain — synthesized from the live DOM tree.
//!
//! There is no platform a11y layer behind this engine; the CDP consumers
//! that matter here (agent-browser's `snapshot`/`@ref` UX, Puppeteer's
//! aria queries) call `Accessibility.getFullAXTree` and map nodes back to
//! the DOM via `backendDOMNodeId`. The synthesis walks the live DomTree and
//! emits Chrome's wire shape — field semantics validated against headless
//! Chrome 152 on a shared fixture: RootWebArea named by the document title,
//! heading/link/button named from subtree text, paragraphs unnamed with
//! StaticText children, a checkbox named by its wrapping label, `checked`
//! as a tristate string, heading level as an integer.
//!
//! Deliberate divergences from Chrome, each invisible to agent-browser's
//! tree builder (which drops ignored nodes and filters the InlineTextBox
//! layer anyway):
//! - no `ignored: true` wrappers for html/body — visible content hangs
//!   directly off RootWebArea;
//! - no InlineTextBox children under StaticText;
//! - `hidden`/`aria-hidden`/non-visual subtrees are omitted rather than
//!   emitted as ignored — same post-filter state, less wire.
//!
//! v1 boundary: light tree only (shadow subtrees are not walked), one
//! frame (the main frame is the only frame).

use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;
use crate::diting_dom::tree::Node;
use crate::diting_dom::{DomTree, NodeData, NodeId};

pub async fn handle(
    method: &str,
    _params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "enable" | "disable" => Ok(json!({})),
        // agent-browser asks for the full tree; `depth`/`frameId` params
        // (Chrome's) are ignored — the main frame is the only frame.
        "getFullAXTree" | "getPartialAXTree" => {
            let page = ctx.get_session_page_mut(session_id).ok_or("No page")?;
            let url = page.url_string();
            let title = page.title.clone();
            let nodes = page
                .with_dom(|dom| build_ax_tree(dom, &title, &url))
                .ok_or("No document")?;
            Ok(json!({ "nodes": nodes }))
        }
        _ => Err(format!("Unknown Accessibility method: {method}")),
    }
}

/// Roles whose accessible name falls back to subtree text (ARIA "name from
/// content"). Paragraphs/divs are absent on purpose: Chrome leaves them
/// unnamed and puts their text on StaticText children instead.
const NAME_FROM_CONTENT: &[&str] = &[
    "button", "link", "heading", "option", "menuitem", "menuitemcheckbox",
    "menuitemradio", "radio", "checkbox", "tab", "treeitem", "cell", "columnheader",
    "rowheader", "searchbox", "slider", "spinbutton", "switch",
];

/// Tags whose subtrees never reach the a11y tree: document metadata, the
/// script host, and text-free containers (svg icons included — a chart that
/// needs a name should carry role/aria-label on a wrapping element).
const SKIPPED_TAGS: &[&str] = &[
    "script", "style", "link", "meta", "title", "base", "template", "noscript",
    "datalist", "head", "svg",
];

struct AxEntry {
    /// DOM NodeId index — doubles as backendDOMNodeId: this engine's DOM
    /// domain shares one id space, and consumers cross the AX→DOM boundary
    /// through backendDOMNodeId (agent-browser's `@ref` map, DOM.* params).
    backend_id: u64,
    role: String,
    name: Option<String>,
    value: Option<String>,
    /// Pre-wrapped AXValue objects, Chrome's wire shape.
    properties: Vec<(String, Value)>,
    child_ids: Vec<String>,
}

impl AxEntry {
    fn to_wire(&self) -> Value {
        let mut v = json!({
            "nodeId": self.backend_id.to_string(),
            "backendDOMNodeId": self.backend_id,
            "ignored": false,
            "role": { "type": "role", "value": self.role },
        });
        if let Some(n) = &self.name {
            v["name"] = json!({ "type": "computedString", "value": n });
        }
        if let Some(val) = &self.value {
            v["value"] = json!({ "type": "string", "value": val });
        }
        if !self.properties.is_empty() {
            v["properties"] = Value::Array(
                self.properties
                    .iter()
                    .map(|(k, pv)| json!({ "name": k, "value": pv }))
                    .collect(),
            );
        }
        v["childIds"] = Value::Array(self.child_ids.iter().map(|c| json!(c)).collect());
        v
    }
}

fn prop_bool(b: bool) -> Value {
    json!({ "type": "boolean", "value": b })
}

fn prop_int(i: i64) -> Value {
    json!({ "type": "integer", "value": i })
}

fn prop_str(s: &str) -> Value {
    json!({ "type": "string", "value": s })
}

/// Chrome reports checkability as a tristate string ("true"/"false"/
/// "mixed"), not a boolean — agents stringify either way, but match the
/// wire shape exactly.
fn prop_tristate(checked: bool) -> Value {
    json!({ "type": "tristate", "value": if checked { "true" } else { "false" } })
}

/// A trimmed, whitespace-collapsed, non-empty name — whitespace-only
/// sources don't name anything.
fn nonempty(s: &str) -> Option<String> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { None } else { Some(t) }
}

/// Collapse each whitespace run to a single space, keeping one leading or
/// trailing space so adjacent StaticText nodes keep their word boundaries.
/// Whitespace-only strings are skipped by the caller (trim check).
fn collapse_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

fn input_type(node: &Node) -> String {
    node.get_attribute("type")
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// input's implicit role by type. `hidden` never reaches here — the
/// subtree skip drops it earlier.
fn input_role(input_type: &str) -> &'static str {
    match input_type {
        "search" => "searchbox",
        "number" => "spinbutton",
        "range" => "slider",
        "checkbox" => "checkbox",
        "radio" => "radio",
        "button" | "submit" | "reset" | "image" | "file" | "color" => "button",
        // text/email/url/tel/password/'' and the date/time family
        _ => "textbox",
    }
}

fn is_text_like(input_type: &str) -> bool {
    !matches!(input_type, "checkbox" | "radio" | "button" | "submit" | "reset" | "image" | "file" | "color" | "range" | "hidden")
}

/// Explicit `role` wins (first token; `presentation`/`none` unrole the
/// element but keep its subtree). Otherwise the tag's implicit mapping —
/// mirroring Chrome's choices where agents look, including the two
/// surprises in agent-browser's vocabulary: capital-I `Iframe` and
/// `LabelText` for label.
fn element_role(node: &Node, tag: &str) -> Option<String> {
    if let Some(explicit) = node.get_attribute("role") {
        let first = explicit
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        return match first.as_str() {
            "" => implicit_role(node, tag).map(str::to_string),
            "presentation" | "none" => None,
            other => Some(other.to_string()),
        };
    }
    implicit_role(node, tag).map(str::to_string)
}

fn implicit_role(node: &Node, tag: &str) -> Option<&'static str> {
    match tag {
        // An anchor is only a link while it navigates (has href); Chrome
        // degrades a bare <a> to generic.
        "a" | "area" => node.get_attribute("href").map(|_| "link"),
        "button" | "summary" => Some("button"),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some("heading"),
        "input" => Some(input_role(&input_type(node))),
        "textarea" => Some("textbox"),
        "select" => Some("combobox"),
        "option" => Some("option"),
        "optgroup" => Some("group"),
        "label" => Some("LabelText"),
        "ul" | "ol" | "dl" => Some("list"),
        "li" => Some("listitem"),
        "dt" => Some("term"),
        "dd" => Some("definition"),
        "table" => Some("table"),
        "caption" => Some("caption"),
        "thead" | "tbody" | "tfoot" => Some("rowgroup"),
        "tr" => Some("row"),
        "td" => Some("cell"),
        "th" => Some(match node
            .get_attribute("scope")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("row") | Some("rowgroup") => "rowheader",
            _ => "columnheader",
        }),
        "nav" => Some("navigation"),
        "main" => Some("main"),
        "article" => Some("article"),
        "aside" => Some("complementary"),
        "header" => Some("banner"),
        "footer" => Some("contentinfo"),
        "form" => Some("form"),
        "img" => Some("img"),
        "iframe" => Some("Iframe"),
        "figure" => Some("figure"),
        "fieldset" | "details" => Some("group"),
        "dialog" => Some("dialog"),
        "video" => Some("video"),
        "audio" => Some("audio"),
        "p" => Some("paragraph"),
        "blockquote" => Some("blockquote"),
        // An unnamed section is just a box; only an authored name makes it
        // a landmark.
        "section" => {
            if node.get_attribute("aria-label").is_some() || node.get_attribute("title").is_some()
            {
                Some("region")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn heading_level(tag: &str) -> Option<i64> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn is_focusable(node: &Node, tag: &str) -> bool {
    if node.get_attribute("tabindex").is_some() {
        return true;
    }
    match tag {
        "button" | "summary" | "select" | "textarea" => true,
        "a" | "area" => node.get_attribute("href").is_some(),
        "input" => input_type(node) != "hidden",
        _ => false,
    }
}

fn parse_bool_attr(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The element children of <body>: html/body themselves never surface
/// (Chrome marks them ignored), so visible content hangs off RootWebArea.
/// Fragments without an html wrapper fall back to the document's children.
fn body_children(dom: &DomTree) -> Vec<NodeId> {
    let is_tag = |id: NodeId, tag: &str| {
        dom.with_node(id, |n| n.as_element().map(|q| q.local.as_ref()) == Some(tag))
            .unwrap_or(false)
    };
    let doc = dom.document();
    for child in dom.children(doc) {
        if is_tag(child, "html") {
            for gc in dom.children(child) {
                if is_tag(gc, "body") {
                    return dom.children(gc);
                }
            }
            // html without body: head children get skipped by tag below.
            return dom.children(child);
        }
    }
    dom.children(doc)
}

struct TreeBuilder<'a> {
    dom: &'a DomTree,
    /// Label text by `for` target id — a form control's name resolves
    /// through this map (label[for]) before walking ancestors for a
    /// wrapping label, mirroring Chrome's native-label precedence.
    label_for: std::collections::HashMap<String, String>,
    nodes: Vec<AxEntry>,
}

pub fn build_ax_tree(dom: &DomTree, title: &str, url: &str) -> Vec<Value> {
    let mut b = TreeBuilder {
        dom,
        label_for: Default::default(),
        nodes: Vec::new(),
    };
    b.index_labels();
    // RootWebArea: Chrome names it by the document title (falling back to
    // the URL for blank pages) and reports the document URL + focusability.
    let root_name = if title.trim().is_empty() {
        url.to_string()
    } else {
        title.to_string()
    };
    let root = b.push(AxEntry {
        backend_id: dom.document().index() as u64,
        role: "RootWebArea".to_string(),
        name: Some(root_name),
        value: None,
        properties: vec![
            ("focusable".to_string(), prop_bool(true)),
            ("url".to_string(), prop_str(url)),
        ],
        child_ids: Vec::new(),
    });
    let mut child_ids = Vec::new();
    for id in body_children(dom) {
        if let Some(backend) = b.visit(id) {
            child_ids.push(backend.to_string());
        }
    }
    b.nodes[root].child_ids = child_ids;
    b.nodes.iter().map(AxEntry::to_wire).collect()
}

impl<'a> TreeBuilder<'a> {
    fn push(&mut self, entry: AxEntry) -> usize {
        self.nodes.push(entry);
        self.nodes.len() - 1
    }

    fn index_labels(&mut self) {
        for id in self.dom.descendants(self.dom.document()) {
            let for_id = self
                .dom
                .with_node(id, |n| {
                    (n.as_element().map(|q| q.local.as_ref()) == Some("label"))
                        .then(|| n.get_attribute("for").map(str::to_string))
                        .flatten()
                })
                .flatten();
            if let Some(for_id) = for_id {
                let text = self.subtree_text(id);
                if !text.is_empty() {
                    self.label_for.insert(for_id, text);
                }
            }
        }
    }

    /// Depth-first walk emitting one AX node per visible element or text
    /// node. Returns the emitted entry's wire id (the DOM NodeId index, the
    /// same space as `nodeId`/`backendDOMNodeId`) for the parent's
    /// childIds.
    fn visit(&mut self, id: NodeId) -> Option<u64> {
        let node = self.dom.get_node(id)?;
        let backend = id.index() as u64;
        match node.data {
            NodeData::Text { contents } => {
                let collapsed = collapse_text(&contents);
                if collapsed.trim().is_empty() {
                    return None;
                }
                self.push(AxEntry {
                    backend_id: backend,
                    role: "StaticText".to_string(),
                    name: Some(collapsed),
                    value: None,
                    properties: Vec::new(),
                    child_ids: Vec::new(),
                });
                Some(backend)
            }
            NodeData::Element { .. } => {
                let tag = node
                    .as_element()
                    .map(|q| q.local.to_string())
                    .unwrap_or_default();
                if self.subtree_hidden(&node, &tag) {
                    return None;
                }
                let role = element_role(&node, &tag).unwrap_or_else(|| "generic".to_string());
                let name = self.accessible_name(&node, id, &tag, &role);
                let value = accessible_value(&node, &role);
                let properties = self.properties_for(&node, &tag, &role);
                let idx = self.push(AxEntry {
                    backend_id: backend,
                    role,
                    name,
                    value,
                    properties,
                    child_ids: Vec::new(),
                });
                let mut child_ids = Vec::new();
                for child in self.dom.children(id) {
                    if let Some(cb) = self.visit(child) {
                        child_ids.push(cb.to_string());
                    }
                }
                self.nodes[idx].child_ids = child_ids;
                Some(backend)
            }
            // Comments, doctypes, processing instructions never surface.
            _ => None,
        }
    }

    fn subtree_hidden(&self, node: &Node, tag: &str) -> bool {
        if SKIPPED_TAGS.contains(&tag) {
            return true;
        }
        if node.get_attribute("hidden").is_some() {
            return true;
        }
        if node
            .get_attribute("aria-hidden")
            .and_then(parse_bool_attr)
            == Some(true)
        {
            return true;
        }
        tag == "input" && input_type(node) == "hidden"
    }

    /// Subtree text with script/style content excluded and whitespace
    /// collapsed — the string an accessible name falls back to.
    fn subtree_text(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        nonempty(&out).unwrap_or_default()
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        let Some(node) = self.dom.get_node(id) else { return };
        match &node.data {
            NodeData::Text { contents } => out.push_str(contents),
            NodeData::Element { name, .. } => {
                if !SKIPPED_TAGS.contains(&name.local.as_ref()) {
                    for child in self.dom.children(id) {
                        self.collect_text(child, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// The accessible name, following the ARIA precedence (labelledby >
    /// label > native label > content) with the native sources Chrome
    /// actually uses: input[value] for the button family, alt for images,
    /// label[for] or a wrapping label for controls, placeholder as the
    /// textbox last resort.
    fn accessible_name(
        &self,
        node: &Node,
        id: NodeId,
        tag: &str,
        role: &str,
    ) -> Option<String> {
        if let Some(referenced) = node.get_attribute("aria-labelledby") {
            let mut parts: Vec<String> = Vec::new();
            for idref in referenced.split_ascii_whitespace() {
                if let Some(target) = self.dom.get_element_by_id(idref) {
                    let text = self.subtree_text(target);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            if !parts.is_empty() {
                return Some(parts.join(" "));
            }
        }
        if let Some(label) = nonempty(node.get_attribute("aria-label").unwrap_or("")) {
            return Some(label);
        }
        let input_t = if tag == "input" { input_type(node) } else { String::new() };
        // <input type=submit value="Save"> is named by its value, like Chrome.
        if matches!(input_t.as_str(), "button" | "submit" | "reset" | "image") {
            if let Some(v) = nonempty(node.get_attribute("value").unwrap_or("")) {
                return Some(v);
            }
        }
        if tag == "img" || input_t == "image" {
            if let Some(alt) = nonempty(node.get_attribute("alt").unwrap_or("")) {
                return Some(alt);
            }
        }
        if matches!(tag, "input" | "textarea" | "select" | "meter" | "progress" | "output") {
            if let Some(control_id) = node.get_attribute("id") {
                if let Some(text) = self.label_for.get(control_id) {
                    return Some(text.clone());
                }
            }
            // Else the nearest wrapping <label>. The control itself
            // contributes no text, so the label's subtree text is its label.
            for ancestor in self.dom.ancestors(id) {
                let is_label = self
                    .dom
                    .with_node(ancestor, |n| {
                        n.as_element().map(|q| q.local.as_ref()) == Some("label")
                    })
                    .unwrap_or(false);
                if is_label {
                    let text = self.subtree_text(ancestor);
                    if !text.is_empty() {
                        return Some(text);
                    }
                    break;
                }
            }
        }
        if (tag == "input" && is_text_like(&input_t)) || tag == "textarea" {
            if let Some(p) = nonempty(node.get_attribute("placeholder").unwrap_or("")) {
                return Some(p);
            }
        }
        if let Some(t) = nonempty(node.get_attribute("title").unwrap_or("")) {
            return Some(t);
        }
        if NAME_FROM_CONTENT.contains(&role) {
            return nonempty(&self.subtree_text(id));
        }
        None
    }

    fn properties_for(&self, node: &Node, tag: &str, role: &str) -> Vec<(String, Value)> {
        let mut props: Vec<(String, Value)> = Vec::new();
        if let Some(level) = heading_level(tag) {
            props.push(("level".to_string(), prop_int(level)));
        }
        if matches!(
            role,
            "checkbox" | "radio" | "menuitemcheckbox" | "menuitemradio" | "switch"
        ) {
            // Dirty checkedness (el.checked = x) beats the parsed attribute
            // — the same precedence the JS getter resolves with.
            let checked = node
                .live_checked()
                .unwrap_or_else(|| node.get_attribute("checked").is_some());
            props.push(("checked".to_string(), prop_tristate(checked)));
        }
        let disabled = node.get_attribute("disabled").is_some()
            || node.get_attribute("aria-disabled").and_then(parse_bool_attr) == Some(true);
        if disabled {
            props.push(("disabled".to_string(), prop_bool(true)));
        }
        if node.get_attribute("required").is_some() {
            props.push(("required".to_string(), prop_bool(true)));
        }
        if let Some(x) = node.get_attribute("aria-expanded").and_then(parse_bool_attr) {
            props.push(("expanded".to_string(), prop_bool(x)));
        }
        if let Some(x) = node.get_attribute("aria-selected").and_then(parse_bool_attr) {
            props.push(("selected".to_string(), prop_bool(x)));
        }
        if tag == "a" {
            if let Some(href) = node.get_attribute("href").filter(|h| !h.trim().is_empty()) {
                props.push(("url".to_string(), prop_str(href)));
            }
        }
        if is_focusable(node, tag) {
            props.push(("focusable".to_string(), prop_bool(true)));
        }
        props
    }
}

/// Live text for value-bearing controls — what the user typed beats the
/// parsed value attribute. Chrome omits empty values; so do we.
fn accessible_value(node: &Node, role: &str) -> Option<String> {
    if !matches!(
        role,
        "textbox" | "searchbox" | "spinbutton" | "combobox" | "slider"
    ) {
        return None;
    }
    let live = match &node.data {
        NodeData::Element { live_value, .. } => live_value.clone(),
        _ => None,
    };
    let v = live.or_else(|| node.get_attribute("value").map(str::to_string))?;
    nonempty(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diting_dom::tree::Attribute;
    use html5ever::{LocalName, Namespace, QualName};

    fn el(dom: &DomTree, tag: &str, attrs: &[(&str, &str)]) -> NodeId {
        dom.new_node(NodeData::Element {
            name: QualName::new(None, Namespace::default(), LocalName::from(tag)),
            attrs: attrs
                .iter()
                .map(|(k, v)| Attribute {
                    name: QualName::new(None, Namespace::default(), LocalName::from(*k)),
                    value: (*v).to_string(),
                })
                .collect(),
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        })
    }

    fn text(dom: &DomTree, s: &str) -> NodeId {
        dom.new_node(NodeData::Text { contents: s.to_string() })
    }

    fn role_of(n: &Value) -> &str {
        n["role"]["value"].as_str().unwrap_or("")
    }

    fn name_of(n: &Value) -> Option<&str> {
        n.get("name").and_then(|v| v["value"].as_str())
    }

    /// A property's scalar value — digs through Chrome's AXValue wrapper
    /// ({name, value: {type, value}}) to the payload.
    fn prop<'a>(n: &'a Value, key: &str) -> Option<&'a Value> {
        n.get("properties")?
            .as_array()?
            .iter()
            .find(|p| p["name"].as_str() == Some(key))
            .and_then(|p| p.get("value"))
            .and_then(|v| v.get("value"))
    }

    fn find<'a>(nodes: &'a [Value], role: &str, name: &str) -> &'a Value {
        nodes
            .iter()
            .find(|n| role_of(n) == role && name_of(n) == Some(name))
            .unwrap_or_else(|| panic!("no {role} named {name:?}"))
    }

    /// The spike fixture shared with the Chrome ground-truth capture
    /// (/tmp/ax-chrome-tree.json): one of every shape agents hit.
    fn spike_dom() -> (DomTree, NodeId) {
        let dom = DomTree::new();
        let html = el(&dom, "html", &[]);
        let head = el(&dom, "head", &[]);
        let title = el(&dom, "title", &[]);
        dom.append_child(dom.document(), html);
        dom.append_child(html, head);
        dom.append_child(head, title);
        dom.append_child(title, text(&dom, "Spike Page"));
        let body = el(&dom, "body", &[]);
        dom.append_child(html, body);

        let h1 = el(&dom, "h1", &[]);
        dom.append_child(body, h1);
        dom.append_child(h1, text(&dom, "aginxbrowser spike"));

        let p = el(&dom, "p", &[("id", "status")]);
        dom.append_child(body, p);
        dom.append_child(p, text(&dom, "not clicked"));

        let b = el(&dom, "button", &[("id", "b1")]);
        dom.append_child(body, b);
        dom.append_child(b, text(&dom, "Buy now"));

        let a = el(&dom, "a", &[("id", "l1"), ("href", "#x")]);
        dom.append_child(body, a);
        dom.append_child(a, text(&dom, "Learn more"));

        let input = el(&dom, "input", &[("id", "t1"), ("type", "text"), ("placeholder", "Email")]);
        dom.append_child(body, input);

        let form = el(&dom, "form", &[]);
        dom.append_child(body, form);
        let label = el(&dom, "label", &[]);
        dom.append_child(form, label);
        dom.append_child(label, text(&dom, " Accept "));
        let cb = el(&dom, "input", &[("id", "c1"), ("type", "checkbox")]);
        dom.append_child(label, cb);

        let script = el(&dom, "script", &[]);
        dom.append_child(body, script);
        dom.append_child(script, text(&dom, "var secret = 1"));

        (dom, b)
    }

    #[test]
    fn root_is_titled_by_document_and_carries_url() {
        let (dom, _) = spike_dom();
        let nodes = build_ax_tree(&dom, "Spike Page", "http://x/");
        let root = &nodes[0];
        assert_eq!(role_of(root), "RootWebArea");
        assert_eq!(name_of(root), Some("Spike Page"));
        assert_eq!(
            prop(root, "url").and_then(Value::as_str),
            Some("http://x/")
        );
        assert_eq!(prop(root, "focusable").and_then(Value::as_bool), Some(true));
        // html/body wrappers never surface: the heading is a direct child.
        let first_child = root["childIds"][0].as_str().unwrap();
        let h1 = nodes
            .iter()
            .find(|n| n["nodeId"].as_str() == Some(first_child))
            .unwrap();
        assert_eq!(role_of(h1), "heading");
    }

    #[test]
    fn chrome_name_and_property_shapes() {
        let (dom, button) = spike_dom();
        let nodes = build_ax_tree(&dom, "Spike Page", "http://x/");

        let h1 = find(&nodes, "heading", "aginxbrowser spike");
        assert_eq!(prop(h1, "level").and_then(Value::as_i64), Some(1));

        // Paragraphs stay unnamed; the text rides a StaticText child.
        let p = nodes
            .iter()
            .find(|n| role_of(n) == "paragraph")
            .expect("paragraph present");
        assert!(p.get("name").is_none());
        let child = p["childIds"][0].as_str().unwrap();
        let st = nodes.iter().find(|n| n["nodeId"].as_str() == Some(child)).unwrap();
        assert_eq!(role_of(st), "StaticText");
        assert_eq!(name_of(st), Some("not clicked"));

        let btn = find(&nodes, "button", "Buy now");
        assert_eq!(btn["backendDOMNodeId"].as_u64(), Some(button.index() as u64));
        assert_eq!(prop(btn, "focusable").and_then(Value::as_bool), Some(true));

        let link = find(&nodes, "link", "Learn more");
        assert_eq!(prop(link, "url").and_then(Value::as_str), Some("#x"));

        // Textbox named by placeholder (Chrome's last-resort source), no
        // value while empty, no checked.
        let tb = find(&nodes, "textbox", "Email");
        assert!(tb.get("value").is_none());
        assert!(prop(tb, "checked").is_none());

        // Checkbox named by its wrapping label, checked as a tristate STRING.
        let cb = find(&nodes, "checkbox", "Accept");
        assert_eq!(prop(cb, "checked").and_then(Value::as_str), Some("false"));
        let checked_prop = cb["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == json!("checked"))
            .unwrap();
        assert_eq!(checked_prop["value"]["type"], "tristate");

        // Script content never surfaces.
        assert!(nodes.iter().all(|n| name_of(n) != Some("var secret = 1")));
        assert!(nodes.iter().any(|n| role_of(n) == "form"));
    }

    #[test]
    fn every_childid_resolves_and_backend_ids_are_unique() {
        let (dom, _) = spike_dom();
        let nodes = build_ax_tree(&dom, "Spike Page", "http://x/");
        let ids: std::collections::HashSet<&str> =
            nodes.iter().filter_map(|n| n["nodeId"].as_str()).collect();
        assert_eq!(ids.len(), nodes.len(), "nodeIds must be unique");
        for n in &nodes {
            for c in n["childIds"].as_array().unwrap() {
                let cid = c.as_str().unwrap();
                assert!(ids.contains(cid), "childId {cid} must resolve");
            }
        }
    }

    #[test]
    fn hidden_and_aria_hidden_subtrees_are_omitted() {
        let dom = DomTree::new();
        let body = el(&dom, "body", &[]);
        dom.append_child(dom.document(), body);
        let vis = el(&dom, "p", &[]);
        dom.append_child(body, vis);
        dom.append_child(vis, text(&dom, "visible"));
        let hidden = el(&dom, "div", &[("hidden", "")]);
        dom.append_child(body, hidden);
        dom.append_child(hidden, text(&dom, "ghost"));
        let aria = el(&dom, "span", &[("aria-hidden", "true")]);
        dom.append_child(body, aria);
        dom.append_child(aria, text(&dom, "decor"));
        let hi = el(&dom, "input", &[("type", "hidden")]);
        dom.append_child(body, hi);

        let nodes = build_ax_tree(&dom, "t", "http://x/");
        assert!(!nodes.iter().any(|n| name_of(n) == Some("ghost")));
        assert!(!nodes.iter().any(|n| name_of(n) == Some("decor")));
        assert!(!nodes.iter().any(|n| role_of(n) == "textbox"));
        assert!(nodes.iter().any(|n| name_of(n) == Some("visible")));
    }

    #[test]
    fn name_precedence_labelledby_beats_label_beats_placeholder() {
        let dom = DomTree::new();
        let body = el(&dom, "body", &[]);
        dom.append_child(dom.document(), body);

        let target = el(&dom, "span", &[("id", "ref1")]);
        dom.append_child(body, target);
        dom.append_child(target, text(&dom, "Referenced Name"));

        let label = el(&dom, "label", &[("for", "t2")]);
        dom.append_child(body, label);
        dom.append_child(label, text(&dom, "Native Label"));
        let label3 = el(&dom, "label", &[("for", "t3")]);
        dom.append_child(body, label3);
        dom.append_child(label3, text(&dom, "Form Label"));

        // labelledby wins over aria-label.
        let a = el(
            &dom,
            "div",
            &[("role", "button"), ("aria-labelledby", "ref1"), ("aria-label", "worse")],
        );
        dom.append_child(body, a);
        // aria-label beats the native label[for].
        let b = el(
            &dom,
            "input",
            &[("id", "t2"), ("type", "text"), ("aria-label", "Aria Wins"), ("placeholder", "ph")],
        );
        dom.append_child(body, b);
        // label[for] beats placeholder.
        let c = el(&dom, "input", &[("id", "t3"), ("type", "text"), ("placeholder", "ph")]);
        dom.append_child(body, c);
        // title beats content for a non-content role; explicit role honored.
        let d = el(&dom, "div", &[("role", "switch"), ("title", "Titled")]);
        dom.append_child(body, d);

        let nodes = build_ax_tree(&dom, "t", "http://x/");
        assert_eq!(name_of(find(&nodes, "button", "Referenced Name")), Some("Referenced Name"));
        assert_eq!(name_of(find(&nodes, "textbox", "Aria Wins")), Some("Aria Wins"));
        assert_eq!(name_of(find(&nodes, "textbox", "Form Label")), Some("Form Label"));
        assert_eq!(role_of(find(&nodes, "textbox", "Form Label")), "textbox");
        assert_eq!(name_of(find(&nodes, "switch", "Titled")), Some("Titled"));
    }

    #[test]
    fn live_value_and_dirty_checked_beat_parsed_attributes() {
        let dom = DomTree::new();
        let body = el(&dom, "body", &[]);
        dom.append_child(dom.document(), body);
        let t = el(&dom, "input", &[("type", "text"), ("placeholder", "Email"), ("value", "stale")]);
        dom.append_child(body, t);
        dom.with_node_mut(t, |n| n.set_live_value("typed@x".to_string()));
        let c = el(&dom, "input", &[("type", "checkbox")]);
        dom.append_child(body, c);
        dom.with_node_mut(c, |n| n.set_live_checked(true));

        let nodes = build_ax_tree(&dom, "t", "http://x/");
        let tb = find(&nodes, "textbox", "Email");
        assert_eq!(tb["value"]["value"].as_str(), Some("typed@x"));
        let cb: &Value = nodes.iter().find(|n| role_of(n) == "checkbox").unwrap();
        assert_eq!(prop(cb, "checked").and_then(Value::as_str), Some("true"));
    }

    #[test]
    fn bare_anchor_degrades_to_generic_but_href_anchor_is_a_link() {
        let dom = DomTree::new();
        let body = el(&dom, "body", &[]);
        dom.append_child(dom.document(), body);
        let bare = el(&dom, "a", &[]);
        dom.append_child(body, bare);
        dom.append_child(bare, text(&dom, "nowhere"));
        let linked = el(&dom, "a", &[("href", "/go")]);
        dom.append_child(body, linked);
        dom.append_child(linked, text(&dom, "somewhere"));

        let nodes = build_ax_tree(&dom, "t", "http://x/");
        find(&nodes, "link", "somewhere");
        assert!(!nodes.iter().any(|n| role_of(n) == "link" && name_of(n) == Some("nowhere")));
        // The degraded anchor is an unnamed generic; its text rides a
        // StaticText child, exactly like Chrome.
        let bare = nodes
            .iter()
            .find(|n| role_of(n) == "generic")
            .expect("bare anchor degrades to generic");
        assert!(name_of(bare).is_none());
        assert!(nodes.iter().any(|n| name_of(n) == Some("nowhere")));
    }
}
