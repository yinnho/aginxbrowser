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

use super::spec::{validate_sequence, validate_workflow, DiagramSpec};
use super::sequence::render_sequence;
use super::workflow::{render_workflow, RouteRepair};

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
    /// disclosed per-edge (workflow only).
    pub repairs: Vec<RouteRepair>,
}

/// A rendered fence, in family-agnostic terms for the figure splice.
struct FenceDiagram {
    kind: &'static str,
    svg: String,
    title: String,
    facts: Vec<String>,
    repairs: Vec<RouteRepair>,
}

/// Parse one fence body. The family is the declared `diagram_type`, or the
/// one object actually present when it is not declared.
fn parse_fence(body: &str) -> Result<FenceDiagram, Vec<String>> {
    let parsed: DiagramSpec =
        serde_json::from_str(body).map_err(|e| vec![format!("JSON parse error: {e}")])?;
    let diagram_type = match (
        parsed.diagram_type.as_deref(),
        parsed.sequence.is_some(),
        parsed.workflow.is_some(),
    ) {
        (Some(t), _, _) => t.to_string(),
        (None, false, true) => "workflow".to_string(),
        (None, _, _) => "sequence".to_string(),
    };
    match (diagram_type.as_str(), parsed.sequence, parsed.workflow) {
        ("sequence", Some(spec), _) => {
            let problems = validate_sequence(&spec);
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_sequence(&spec)?;
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
            })
        }
        ("workflow", _, Some(spec)) => {
            let problems = validate_workflow(&spec);
            if !problems.is_empty() {
                return Err(problems);
            }
            let r = render_workflow(&spec)?;
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
            })
        }
        ("sequence", None, _) => Err(vec![
            "a sequence diagram needs a \"sequence\" object with title, participants, messages"
                .to_string(),
        ]),
        ("workflow", None, _) => Err(vec![
            "a workflow diagram needs a \"workflow\" object with title, lanes, nodes, edges"
                .to_string(),
        ]),
        (other, _, _) => Err(vec![format!(
            "unknown diagram_type \"{other}\" — v1 renders \"sequence\" and \"workflow\""
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

pub fn render(markdown: &str) -> RenderedDoc {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let events: Vec<Event> = Parser::new_ext(markdown, options).collect();

    let mut body = String::with_capacity(markdown.len() + 1024);
    let mut fences: Vec<FenceOutcome> = Vec::new();
    let mut doc_title: Option<String> = None;

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
                    match parse_fence(&raw) {
                        Ok(d) => {
                            body.push_str(&format!(
                                "<figure class=\"agx-diagram\" data-diagram-type=\"{}\" data-diagram-index=\"{index}\" data-diagram-title=\"{}\">{}</figure>",
                                d.kind,
                                html_escape(&d.title),
                                d.svg
                            ));
                            fences.push(FenceOutcome {
                                index,
                                ok: true,
                                kind: d.kind,
                                title: Some(d.title.clone()),
                                detail: d.facts,
                                repairs: d.repairs,
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

    let title = doc_title.unwrap_or_else(|| "Document".to_string());
    let html = format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>{}</style>\n</head>\n<body>\n{}</body>\n</html>\n",
        html_escape(&title),
        SHELL_CSS,
        body
    );
    RenderedDoc { html, fences }
}

/// The plain-Jane shell (批2: 素颜). One style element, no fonts fetched,
/// no scripts — the artifact is offline and self-contained by construction.
const SHELL_CSS: &str = "body{max-width:920px;margin:2rem auto;padding:0 1rem;font:16px/1.65 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;color:#18181b;background:#fff}figure.agx-diagram{margin:2.5rem 0}figure.agx-diagram svg{width:100%;height:auto;display:block}pre{background:#f4f4f5;padding:1rem 1.25rem;border-radius:8px;overflow-x:auto;font-size:.875rem;line-height:1.5}code,pre,kbd,samp{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}code{background:#f4f4f5;padding:.1em .35em;border-radius:4px;font-size:.875em}pre code{background:none;padding:0}table{border-collapse:collapse;margin:1rem 0}th,td{border:1px solid #e4e4e7;padding:.375rem .625rem;text-align:left}img{max-width:100%}blockquote{margin:1rem 0;padding:.25rem 1rem;border-left:3px solid #e4e4e7;color:#52525b}h1,h2{line-height:1.25}hr{border:none;border-top:1px solid #e4e4e7;margin:2rem 0}";

#[cfg(test)]
mod tests {
    use super::*;

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
}
