//! The markdown shell: prose streams through pulldown-cmark's own HTML
//! emitter; ```archify fenced blocks are intercepted, parsed as typed JSON,
//! and replaced by rendered diagram figures. A fence that fails to parse or
//! validate falls back to an ordinary code block — the document stays
//! renderable and the failure lands in the receipt as a diagnostic.
//!
//! Degenerate shapes fall out for free: pure prose renders a prose-only
//! document; a single bare fence renders a diagram-only artifact (the
//! "just give me JSON" entry point).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use pulldown_cmark::html::push_html;

use super::architecture::render_architecture;
use super::dataflow::render_dataflow;
use super::graph::RouteRepair;
use super::lifecycle::render_lifecycle;
use super::sequence::render_sequence;
use super::spec::{
    validate_architecture, validate_dataflow, validate_lifecycle, validate_sequence,
    validate_views, validate_workflow, DiagramSpec, View,
};
use super::theme::Theme;
use super::workflow::render_workflow;

/// The info string that routes a fence to the diagram pipeline.
const FENCE_LANG: &str = "archify";

pub struct FenceOutcome {
    /// 0-based fence ordinal in document order.
    pub index: usize,
    pub ok: bool,
    /// Diagram family ("sequence"/"workflow"); empty when the fence failed
    /// before routing resolved.
    pub kind: &'static str,
    pub title: Option<String>,
    /// Structured facts for the receipt when ok; problems when not.
    pub detail: Vec<String>,
    /// Route presets the engine substituted on this diagram's behalf,
    /// disclosed per-edge (the graph-routed families).
    pub repairs: Vec<RouteRepair>,
    /// Composition audit over the placed geometry; None when the fence
    /// failed before a diagram existed.
    pub composition: Option<super::checks::Composition>,
    /// Guided views carried by the fence (empty when none): named node
    /// subsets the viewer offers as tabs.
    pub views: Vec<View>,
}

/// A rendered fence, in family-agnostic terms for the figure splice.
struct FenceDiagram {
    kind: &'static str,
    svg: String,
    title: String,
    facts: Vec<String>,
    repairs: Vec<RouteRepair>,
    composition: super::checks::Composition,
    views: Vec<View>,
}

/// Parse one fence body. The family is the declared `diagram_type`, or the
/// one object actually present when it is not declared.
fn parse_fence(body: &str, theme: &'static Theme) -> Result<FenceDiagram, Vec<String>> {
    let parsed: DiagramSpec =
        serde_json::from_str(body).map_err(|e| vec![format!("JSON parse error: {e}")])?;
    // Guided views ride at the envelope (family-agnostic); validation
    // resolves their nodes against whichever family the fence routes to.
    let views = parsed.views.clone().unwrap_or_default();
    let diagram_type = match (
        parsed.diagram_type.as_deref(),
        parsed.sequence.is_some(),
        parsed.workflow.is_some(),
        parsed.dataflow.is_some(),
        parsed.lifecycle.is_some(),
        parsed.architecture.is_some(),
    ) {
        (Some(t), _, _, _, _, _) => t.to_string(),
        (None, false, false, false, false, true) => "architecture".to_string(),
        (None, false, false, false, true, false) => "lifecycle".to_string(),
        (None, false, false, true, false, false) => "dataflow".to_string(),
        (None, false, true, false, false, false) => "workflow".to_string(),
        (None, _, _, _, _, _) => "sequence".to_string(),
    };
    match (
        diagram_type.as_str(),
        &parsed.sequence,
        &parsed.workflow,
        &parsed.dataflow,
        &parsed.lifecycle,
        &parsed.architecture,
    ) {
        ("sequence", Some(spec), ..) => {
            let mut problems = validate_sequence(spec);
            let ids: Vec<&str> = spec.participants.iter().map(|p| p.id.as_str()).collect();
            problems.extend(validate_views(&views, &ids));
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_sequence(spec, theme)?;
            Ok(FenceDiagram {
                kind: "sequence",
                facts: vec![
                    format!("participants={}", r.participants),
                    format!("messages={}", r.messages),
                    format!("viewBox={}x{}", r.view_box[0], r.view_box[1]),
                ],
                svg: r.svg,
                title: r.title,
                repairs: Vec::new(),
                composition: r.composition,
                views,
            })
        }
        ("workflow", _, Some(spec), ..) => {
            let mut problems = validate_workflow(spec);
            let ids: Vec<&str> = spec.nodes.iter().map(|n| n.id.as_str()).collect();
            problems.extend(validate_views(&views, &ids));
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_workflow(spec, theme)?;
            Ok(FenceDiagram {
                kind: "workflow",
                facts: vec![
                    format!("lanes={}", r.lanes),
                    format!("nodes={}", r.nodes),
                    format!("edges={}", r.edges),
                    format!("viewBox={}x{}", r.view_box[0], r.view_box[1]),
                ],
                svg: r.svg,
                title: r.title,
                repairs: r.repairs,
                composition: r.composition,
                views,
            })
        }
        ("dataflow", _, _, Some(spec), ..) => {
            let mut problems = validate_dataflow(spec);
            let ids: Vec<&str> = spec.nodes.iter().map(|n| n.id.as_str()).collect();
            problems.extend(validate_views(&views, &ids));
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_dataflow(spec, theme)?;
            Ok(FenceDiagram {
                kind: "dataflow",
                facts: vec![
                    format!("stages={}", r.stages),
                    format!("nodes={}", r.nodes),
                    format!("flows={}", r.flows),
                    format!("viewBox={}x{}", r.view_box[0], r.view_box[1]),
                ],
                svg: r.svg,
                title: r.title,
                repairs: r.repairs,
                composition: r.composition,
                views,
            })
        }
        ("lifecycle", _, _, _, Some(spec), _) => {
            let mut problems = validate_lifecycle(spec);
            let ids: Vec<&str> = spec.states.iter().map(|s| s.id.as_str()).collect();
            problems.extend(validate_views(&views, &ids));
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_lifecycle(spec, theme)?;
            Ok(FenceDiagram {
                kind: "lifecycle",
                facts: vec![
                    format!("lanes={}", r.lanes),
                    format!("states={}", r.states),
                    format!("transitions={}", r.transitions),
                    format!("viewBox={}x{}", r.view_box[0], r.view_box[1]),
                ],
                svg: r.svg,
                title: r.title,
                repairs: r.repairs,
                composition: r.composition,
                views,
            })
        }
        ("architecture", .., Some(spec)) => {
            let mut problems = validate_architecture(spec);
            let ids: Vec<&str> = spec.components.iter().map(|c| c.id.as_str()).collect();
            problems.extend(validate_views(&views, &ids));
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_architecture(spec, theme)?;
            Ok(FenceDiagram {
                kind: "architecture",
                facts: vec![
                    format!("components={}", r.components),
                    format!("boundaries={}", r.boundaries),
                    format!("connections={}", r.connections),
                    format!("viewBox={}x{}", r.view_box[0], r.view_box[1]),
                ],
                svg: r.svg,
                title: r.title,
                repairs: r.repairs,
                composition: r.composition,
                views,
            })
        }
        ("sequence", None, ..) => Err(vec![
            "a sequence diagram needs a \"sequence\" object with title, participants, messages"
                .to_string(),
        ]),
        ("workflow", _, None, ..) => Err(vec![
            "a workflow diagram needs a \"workflow\" object with title, lanes, nodes, edges"
                .to_string(),
        ]),
        ("dataflow", _, _, None, ..) => Err(vec![
            "a dataflow diagram needs a \"dataflow\" object with title, stages, nodes, flows"
                .to_string(),
        ]),
        ("lifecycle", _, _, _, None, _) => Err(vec![
            "a lifecycle diagram needs a \"lifecycle\" object with title, lanes, states, transitions"
                .to_string(),
        ]),
        ("architecture", .., None) => Err(vec![
            "an architecture diagram needs an \"architecture\" object with title, components, boundaries, connections"
                .to_string(),
        ]),
        (other, ..) => Err(vec![format!(
            "unknown diagram_type \"{other}\" — v1 renders \"sequence\", \"workflow\", \"dataflow\", \"lifecycle\", and \"architecture\""
        )]),
    }
}

pub struct RenderedDoc {
    pub html: String,
    pub fences: Vec<FenceOutcome>,
}

/// Escapes text for HTML content/attribute contexts.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render(markdown: &str, theme: &'static Theme) -> RenderedDoc {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let events: Vec<Event> = Parser::new_ext(markdown, options).collect();

    let mut body = String::with_capacity(markdown.len() + 1024);
    let mut fences: Vec<FenceOutcome> = Vec::new();
    let mut doc_title: Option<String> = None;
    // Flips when any fence carries views; the viewer script ships once per
    // document at the tail (figures without views have nothing to click).
    let mut has_viewer = false;

    // Plain events accumulate here and flush through push_html whenever an
    // archify figure splices in (push_html emits a whole run at once).
    let mut plain: Vec<Event> = Vec::new();
    let flush = |plain: &mut Vec<Event>, body: &mut String| {
        if !plain.is_empty() {
            push_html(body, plain.drain(..));
        }
    };

    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let first_token = info.split([',', ' ', '\t']).next().unwrap_or("");
                if first_token == FENCE_LANG {
                    // Capture the body: events until the matching End are
                    // Text chunks (code blocks hold no nested markup).
                    let mut raw = String::new();
                    let mut j = i + 1;
                    while j < events.len() && !matches!(events[j], Event::End(TagEnd::CodeBlock)) {
                        if let Event::Text(t) = &events[j] {
                            raw.push_str(t);
                        }
                        j += 1;
                    }
                    flush(&mut plain, &mut body);
                    let index = fences.len();
                    // Parse errors, then spec validation (document-order
                    // problems), then the adapter's geometry checks — the
                    // family router chains all three.
                    match parse_fence(&raw, theme) {
                        Ok(d) => {
                            // Guided views: the tab strip and the JSON data
                            // island ride inside the figure, before the svg.
                            // The island's payload escapes `<` so a label can
                            // never close the script tag early.
                            let tabs = if d.views.is_empty() {
                                String::new()
                            } else {
                                has_viewer = true;
                                let mut t = String::from(
                                    "<div class=\"agx-views\" role=\"tablist\">\
                                     <button type=\"button\" class=\"agx-view is-on\" \
                                     data-view-id=\"\" aria-selected=\"true\">All</button>",
                                );
                                for v in &d.views {
                                    t.push_str(&format!(
                                        "<button type=\"button\" class=\"agx-view\" \
                                         data-view-id=\"{}\" aria-selected=\"false\">{}</button>",
                                        html_escape(&v.id),
                                        html_escape(&v.label)
                                    ));
                                }
                                t.push_str("</div><script type=\"application/json\" \
                                            class=\"agx-views-data\">");
                                let json = serde_json::to_string(&d.views)
                                    .unwrap_or_default()
                                    .replace('<', "\\u003c");
                                t.push_str(&json);
                                t.push_str("</script>");
                                t
                            };
                            body.push_str(&format!(
                                "<figure class=\"agx-diagram\" data-diagram-type=\"{}\" data-diagram-index=\"{index}\" data-diagram-title=\"{}\">{}{}</figure>",
                                d.kind,
                                html_escape(&d.title),
                                tabs,
                                d.svg
                            ));
                            fences.push(FenceOutcome {
                                index,
                                ok: true,
                                kind: d.kind,
                                title: Some(d.title.clone()),
                                detail: d.facts,
                                repairs: d.repairs,
                                composition: Some(d.composition),
                                views: d.views,
                            });
                            if doc_title.is_none() {
                                doc_title = Some(d.title);
                            }
                        }
                        Err(problems) => {
                            // Fall back to a visible code block so the
                            // document survives and the agent can see what
                            // failed against the diagnostics in the receipt.
                            body.push_str(&format!(
                                "<pre><code class=\"language-{FENCE_LANG}\">{}</code></pre>",
                                html_escape(&raw)
                            ));
                            fences.push(FenceOutcome {
                                index,
                                ok: false,
                                kind: "",
                                title: None,
                                detail: problems,
                                repairs: Vec::new(),
                                composition: None,
                                views: Vec::new(),
                            });
                        }
                    }
                    i = j + 1;
                    continue;
                }
                plain.push(events[i].clone());
            }
            Event::Start(Tag::Heading { level, .. }) if *level == HeadingLevel::H1 && doc_title.is_none() => {
                let mut title = String::new();
                let mut j = i + 1;
                while j < events.len() && !matches!(events[j], Event::End(TagEnd::Heading(_))) {
                    if let Event::Text(t) = &events[j] {
                        title.push_str(t);
                    }
                    j += 1;
                }
                doc_title = Some(title.trim().to_string());
                plain.push(events[i].clone());
            }
            _ => plain.push(events[i].clone()),
        }
        i += 1;
    }
    flush(&mut plain, &mut body);
    if has_viewer {
        body.push_str("<script>");
        body.push_str(VIEWER_JS);
        body.push_str("</script>");
    }

    let title = doc_title.unwrap_or_else(|| "Document".to_string());
    let html = format!(
        "<!DOCTYPE html>\n<html data-theme=\"{}\" data-preset=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>{}</style>\n</head>\n<body>\n{}</body>\n</html>\n",
        theme.name,
        theme.preset,
        html_escape(&title),
        shell_css(theme),
        body
    );
    RenderedDoc { html, fences }
}

/// The inlined viewer runtime, shipped once per document at the tail when
/// any fence carried views. Pure ES5, no dependencies; it dims via the
/// `opacity` presentation attribute (the engine's svg paint reads attrs, and
/// opacity multiplies into every paint's alpha). Views light the member
/// nodes plus routes whose endpoints are both members (subgraph); focus
/// lights a node, its neighbors, and the routes touching it (ego). The
/// artifact is fully static without this script — clicking is progressive.
const VIEWER_JS: &str = r#""use strict";
(function () {
  var DIM = "0.12";
  var REG = [];
  function setOp(el, lit) {
    if (lit) { el.removeAttribute("opacity"); }
    else { el.setAttribute("opacity", DIM); }
  }
  function initFig(fig, idx) {
    var svg = fig.querySelector("svg");
    if (!svg) { return; }
    // Views payload rides as a JSON data island inside the figure; absent
    // or corrupt islands leave the diagram inert (no tabs were emitted).
    var views = null;
    var dataTag = fig.querySelector("script.agx-views-data");
    if (dataTag) {
      try { views = JSON.parse(dataTag.textContent || "[]"); }
      catch (e) { views = null; }
    }
    var nodes = svg.querySelectorAll("[data-node-id],[data-participant-id]");
    var routes = svg.querySelectorAll("[data-from][data-to]");
    var mode = "all"; // "all" | "view" | "focus"
    var viewId = null;
    var focusId = null;

    function nodeIdOf(el) {
      return el.getAttribute("data-node-id") ||
        el.getAttribute("data-participant-id");
    }
    function litSet() {
      var lit = {};
      var i;
      if (mode === "all") {
        for (i = 0; i < nodes.length; i++) { lit[nodeIdOf(nodes[i])] = true; }
        return lit;
      }
      if (mode === "view" && views) {
        for (i = 0; i < views.length; i++) {
          if (views[i].id === viewId) {
            var ns = views[i].nodes || [];
            for (var j = 0; j < ns.length; j++) { lit[ns[j]] = true; }
          }
        }
        return lit;
      }
      if (mode === "focus") {
        lit[focusId] = true;
        for (i = 0; i < routes.length; i++) {
          var f = routes[i].getAttribute("data-from");
          var t = routes[i].getAttribute("data-to");
          if (f === focusId) { lit[t] = true; }
          if (t === focusId) { lit[f] = true; }
        }
      }
      return lit;
    }
    function routeOn(lit, f, t) {
      if (mode === "view") { return !!(lit[f] && lit[t]); }
      // Focus lights the routes that touch the focused node — an edge
      // between two of its neighbors is not part of the ego graph.
      if (mode === "focus") { return f === focusId || t === focusId; }
      return true;
    }
    function apply() {
      var dimming = mode !== "all";
      var lit = litSet();
      var i;
      for (i = 0; i < nodes.length; i++) {
        setOp(nodes[i], !dimming || !!lit[nodeIdOf(nodes[i])]);
      }
      for (i = 0; i < routes.length; i++) {
        var on = routeOn(lit,
          routes[i].getAttribute("data-from"),
          routes[i].getAttribute("data-to"));
        setOp(routes[i], !dimming || on);
      }
      // Furniture (title, lanes, legend): direct svg children carrying no
      // node/route data dim with the background.
      var kids = svg.childNodes;
      for (i = 0; i < kids.length; i++) {
        var el = kids[i];
        if (el.nodeType !== 1) { continue; }
        if (el.hasAttribute("data-node-id") ||
            el.hasAttribute("data-participant-id") ||
            el.hasAttribute("data-from")) { continue; }
        setOp(el, !dimming);
      }
      var btns = fig.querySelectorAll("button.agx-view");
      for (i = 0; i < btns.length; i++) {
        var vid = btns[i].getAttribute("data-view-id") || "";
        var mine = (mode === "all" && vid === "") ||
                   (mode === "view" && vid === viewId);
        btns[i].className = mine ? "agx-view is-on" : "agx-view";
        btns[i].setAttribute("aria-selected", mine ? "true" : "false");
      }
    }
    function setFocus(id) {
      if (id === null || id === undefined ||
          (mode === "focus" && focusId === id)) {
        mode = "all"; focusId = null;
      } else { mode = "focus"; focusId = id; viewId = null; }
      apply();
    }
    function setView(id) {
      if (id === null || id === undefined || id === "" ||
          (mode === "view" && viewId === id)) {
        mode = "all"; viewId = null;
      } else { mode = "view"; viewId = id; focusId = null; }
      apply();
    }
    function state() {
      var lit = litSet();
      var litNodes = [];
      var litRoutes = [];
      var i;
      for (i = 0; i < nodes.length; i++) {
        var id = nodeIdOf(nodes[i]);
        if (lit[id] && litNodes.indexOf(id) < 0) { litNodes.push(id); }
      }
      for (i = 0; i < routes.length; i++) {
        var f = routes[i].getAttribute("data-from");
        var t = routes[i].getAttribute("data-to");
        // One route paints as several elements (path + label group): report
        // the edge once.
        var key = f + "->" + t;
        if (routeOn(lit, f, t) && litRoutes.indexOf(key) < 0) {
          litRoutes.push(key);
        }
      }
      return {
        mode: mode, view: viewId, focus: focusId,
        views: views || [], lit: litNodes, litRoutes: litRoutes,
        dimmed: mode !== "all"
      };
    }

    var btns = fig.querySelectorAll("button.agx-view");
    var i;
    for (i = 0; i < btns.length; i++) {
      (function (b) {
        b.addEventListener("click", function () {
          setView(b.getAttribute("data-view-id"));
        });
      })(btns[i]);
    }
    for (i = 0; i < nodes.length; i++) {
      (function (el) {
        el.addEventListener("click", function () { setFocus(nodeIdOf(el)); });
      })(nodes[i]);
    }
    REG[idx] = { focus: setFocus, view: setView, state: state };
  }
  if (!window.agxViewer) {
    window.agxViewer = {
      focus: function (i, id) { if (REG[i]) { REG[i].focus(id); } },
      view: function (i, id) { if (REG[i]) { REG[i].view(id); } },
      state: function (i) { return REG[i] ? REG[i].state() : null; }
    };
  }
  var figs = document.querySelectorAll("figure.agx-diagram");
  for (var i = 0; i < figs.length; i++) { initFig(figs[i], i); }
})();
"#;

/// The shell stylesheet with the theme's five prose colors baked in. No CSS
/// custom properties: the artifact is a static deterministic file rendered
/// by diting, whose SVG paint reads presentation attributes — theme values
/// interpolate at generation time, not runtime (archify's data-theme +
/// custom-property re-theming is the future viewer runtime's mechanism, and
/// the data-theme attribute above is its hook).
fn shell_css(t: &Theme) -> String {
    format!("body{{max-width:920px;margin:2rem auto;padding:0 1rem;font:16px/1.65 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;color:{page_fg};background:{page_bg}}}figure.agx-diagram{{margin:2.5rem 0}}figure.agx-diagram svg{{width:100%;height:auto;display:block}}.agx-views{{margin:0 0 .75rem}}button.agx-view{{display:inline-block;font-family:inherit;font-size:12px;font-weight:600;color:{page_fg};background:{code_bg};border:1px solid {border};border-radius:999px;padding:.25rem .7rem;margin:0 .25rem .25rem 0;cursor:pointer}}button.agx-view.is-on{{background:{page_fg};color:{page_bg};border-color:{page_fg}}}pre{{background:{code_bg};padding:1rem 1.25rem;border-radius:8px;overflow-x:auto;font-size:.875rem;line-height:1.5}}code,pre,kbd,samp{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}code{{background:{code_bg};padding:.1em .35em;border-radius:4px;font-size:.875em}}pre code{{background:none;padding:0}}table{{border-collapse:collapse;margin:1rem 0}}th,td{{border:1px solid {border};padding:.375rem .625rem;text-align:left}}img{{max-width:100%}}blockquote{{margin:1rem 0;padding:.25rem 1rem;border-left:3px solid {border};color:{quote_fg}}}h1,h2{{line-height:1.25}}hr{{border:none;border-top:1px solid {border};margin:2rem 0}}",
        page_fg = t.page_fg,
        page_bg = t.page_bg,
        code_bg = t.code_bg,
        border = t.border,
        quote_fg = t.quote_fg,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::theme::LIGHT;

    // Every shell test rides the default (light) theme; theme behavior has
    // its own coverage in mod/theme tests.
    fn render(markdown: &str) -> RenderedDoc {
        super::render(markdown, &LIGHT)
    }

    const SEQ: &str = r#"{"sequence":{"title":"Ping","participants":[
        {"id":"a","type":"frontend","label":"Client"},
        {"id":"b","type":"backend","label":"Server"}],
       "messages":[{"from":"a","to":"b","label":"ping"}]}}"#;

    #[test]
    fn prose_and_diagram_mix() {
        let doc = render(&format!(
            "# Notes\n\nSome prose with `code`.\n\n```archify\n{SEQ}\n```\n\nMore prose.\n"
        ));
        assert!(doc.html.starts_with("<!DOCTYPE html>"));
        assert!(doc.html.contains("<title>Notes</title>"));
        assert!(doc.html.contains("<figure class=\"agx-diagram\""));
        assert!(doc.html.contains("Some prose"));
        assert_eq!(doc.fences.len(), 1);
        assert!(doc.fences[0].ok);
        assert_eq!(doc.fences[0].title.as_deref(), Some("Ping"));
    }

    #[test]
    fn single_fence_is_a_diagram_only_artifact() {
        let doc = render(&format!("```archify\n{SEQ}\n```\n"));
        assert!(doc.html.contains("<figure"));
        assert!(!doc.html.contains("<p>"));
        // No H1 anywhere: title falls back to the diagram's own title.
        assert!(doc.html.contains("<title>Ping</title>"));
    }

    #[test]
    fn other_code_blocks_pass_through() {
        let doc = render("```rust\nfn main() {}\n```\n");
        assert!(doc.html.contains("language-rust"));
        assert!(doc.html.contains("fn main() {}"));
        assert!(doc.fences.is_empty());
    }

    #[test]
    fn broken_fence_falls_back_and_diagnoses() {
        let doc = render("```archify\n{\"sequence\": {\"title\": \"X\"}}\n```\n");
        assert_eq!(doc.fences.len(), 1);
        assert!(!doc.fences[0].ok);
        assert!(doc.fences[0].detail[0].contains("participants"));
        // The body still renders the fence as a code block (push_html entity-
        // escapes its text).
        assert!(doc.html.contains("language-archify"));
        assert!(doc.html.contains("&quot;sequence&quot;"));
    }

    #[test]
    fn malformed_json_is_a_parse_diagnostic() {
        let doc = render("```archify\n{not json\n```\n");
        assert!(!doc.fences[0].ok);
        assert!(doc.fences[0].detail[0].contains("JSON parse error"));
        assert!(doc.html.contains("language-archify"));
    }

    #[test]
    fn tables_strikethrough_enabled() {
        let doc = render("| a | b |\n|---|---|\n| 1 | 2 |\n\n~~gone~~\n");
        assert!(doc.html.contains("<table>"));
        assert!(doc.html.contains("<del>gone</del>"));
    }

    #[test]
    fn deterministic_bytes_across_calls() {
        let md = format!("# T\n\n```archify\n{SEQ}\n```\n\nparagraph.\n");
        assert_eq!(render(&md).html, render(&md).html);
    }

    // The same sequence fence plus a guided view — views ride the envelope.
    fn seq_with_views() -> String {
        let base = SEQ.strip_suffix('}').unwrap();
        format!(
            "{base},\"views\":[{{\"id\":\"client\",\"label\":\"Client side\",\"nodes\":[\"a\"]}}]}}"
        )
    }

    #[test]
    fn views_emit_tabs_island_and_one_viewer() {
        let doc = render(&format!("```archify\n{}\n```\n", seq_with_views()));
        assert!(doc.html.contains("<div class=\"agx-views\" role=\"tablist\">"));
        assert!(doc
            .html
            .contains("data-view-id=\"client\" aria-selected=\"false\">Client side</button>"));
        assert!(doc
            .html
            .contains("<script type=\"application/json\" class=\"agx-views-data\">"));
        assert!(doc.html.contains("window.agxViewer"));
        // Exactly two script tags: the JSON island inside the figure and the
        // viewer at the tail (the island's payload can never add one).
        assert_eq!(doc.html.matches("<script").count(), 2);
        assert_eq!(doc.html.matches("</script>").count(), 2);
        assert_eq!(doc.fences[0].views.len(), 1);
        assert_eq!(doc.fences[0].views[0].id, "client");
    }

    #[test]
    fn views_absent_ships_no_viewer() {
        let doc = render(&format!("```archify\n{SEQ}\n```\n"));
        // The tab styles ride the stylesheet unconditionally; what must be
        // absent is the strip, the island, and the script.
        assert!(!doc.html.contains("<div class=\"agx-views\""));
        assert!(!doc.html.contains("agx-views-data"));
        assert!(!doc.html.contains("agxViewer"));
        assert!(!doc.html.contains("<script"));
        assert!(doc.fences[0].views.is_empty());
    }

    #[test]
    fn view_payload_cannot_close_the_island() {
        // A label carrying markup lands JSON-escaped inside the island.
        let hostile = r#"{"sequence":{"title":"Ping","participants":[
            {"id":"a","type":"frontend","label":"A"},
            {"id":"b","type":"backend","label":"B"}],
           "messages":[{"from":"a","to":"b","label":"ping"}]},
          "views":[{"id":"x","label":"</script><b>hi","nodes":["a"]}]}"#;
        let doc = render(&format!("```archify\n{hostile}\n```\n"));
        assert!(doc.html.contains("\\u003c/script>"));
        // Still exactly the island + viewer closers, nothing injected.
        assert_eq!(doc.html.matches("</script>").count(), 2);
    }

    fn svg_of(html: &str) -> &str {
        let start = html.find("<svg").unwrap();
        let end = html.find("</svg>").unwrap() + "</svg>".len();
        &html[start..end]
    }

    #[test]
    fn views_do_not_touch_the_diagram_bytes() {
        let with = render(&format!("```archify\n{}\n```\n", seq_with_views()));
        let without = render(&format!("```archify\n{SEQ}\n```\n"));
        assert_eq!(
            svg_of(&with.html),
            svg_of(&without.html),
            "views are shell chrome, never diagram geometry"
        );
    }

    #[test]
    fn unknown_view_node_is_a_diagnostic() {
        let bad = r#"{"sequence":{"title":"Ping","participants":[
            {"id":"a","type":"frontend","label":"A"},
            {"id":"b","type":"backend","label":"B"}],
           "messages":[{"from":"a","to":"b","label":"ping"}]},
          "views":[{"id":"v","label":"Ghost","nodes":["ghost"]}]}"#;
        let doc = render(&format!("```archify\n{bad}\n```\n"));
        assert!(!doc.fences[0].ok);
        assert!(doc.fences[0].detail[0].contains("unknown node \"ghost\""));
        // The fence falls back to a code block: no strip, no island, no viewer.
        assert!(!doc.html.contains("<div class=\"agx-views\""));
        assert!(!doc.html.contains("agx-views-data"));
        assert!(!doc.html.contains("agxViewer"));
    }
}
