// Range / inline client-rect geometry ops, backed by diting_layout::text_geom
// (the painted text items re-wrapped with the painter's own breaker).
use super::*;

/// `text_rects`: arg1 = text-node id, arg2 = "from,to" (node-local char
/// offsets) -> JSON [[x,y,w,h],...] in document coordinates, one per line the
/// span touches. `elem_rects`: arg1 = element id -> one box per line of an
/// inline element's text (a block/replaced element answers "null": the caller
/// keeps its single border box).
#[cfg(feature = "screenshot")]
pub(super) fn run(gs: &JsState, dom: &DomTree, cmd: &str, arg1: &str, arg2: &str) -> String {
    let Some(nid) = arg1.parse::<u32>().ok().map(NodeId::new) else { return "null".into() };
    let epoch = dom.epoch();
    ensure_layout_run(gs, dom, epoch);
    let cache = gs.layout_cache.borrow();
    let Some((e, run)) = cache.as_ref() else { return "null".into() };
    if *e != epoch {
        return "null".into();
    }
    let (_, _, styles, items, ..) = run;
    let fonts_arc = crate::diting_fonts::font_book();
    let fonts: &crate::diting_layout::text::FontBook = &fonts_arc;
    let json = |rects: Vec<[f32; 4]>| {
        serde_json::Value::Array(
            rects.into_iter().map(|r| serde_json::json!([r[0], r[1], r[2], r[3]])).collect(),
        )
        .to_string()
    };
    match cmd {
        "text_rects" => {
            let (from, to) = arg2.split_once(',').and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))).unwrap_or((0, u32::MAX));
            let mut out = Vec::new();
            for f in crate::diting_layout::text_geom::node_frags(items, fonts, nid) {
                // A collapsed range still reports the caret box on its line.
                let hit = if from == to { f.start <= from && from <= f.end } else { f.start < to && from < f.end };
                if hit {
                    out.push(f.sub_rect(fonts, from, to));
                }
            }
            json(out)
        }
        "elem_rects" => {
            let inline = styles.get(&nid).is_some_and(|s| s.display == Some(crate::diting_css::Display::Inline));
            if !inline {
                return "null".into();
            }
            let mut lines: Vec<[f32; 4]> = Vec::new();
            for d in dom.descendants(nid) {
                if !dom.with_node(d, |n| n.is_text()).unwrap_or(false) {
                    continue;
                }
                for f in crate::diting_layout::text_geom::node_frags(items, fonts, d) {
                    let r = f.rect();
                    // Merge into the line box sharing this baseline band.
                    match lines.iter_mut().find(|l| (l[1] - r[1]).abs() < 0.5) {
                        Some(l) => {
                            let x0 = l[0].min(r[0]);
                            let x1 = (l[0] + l[2]).max(r[0] + r[2]);
                            l[0] = x0;
                            l[2] = x1 - x0;
                        }
                        None => lines.push(r),
                    }
                }
            }
            if lines.is_empty() { "null".into() } else { json(lines) }
        }
        _ => "null".into(),
    }
}

#[cfg(not(feature = "screenshot"))]
pub(super) fn run(_gs: &JsState, _dom: &DomTree, _cmd: &str, _a1: &str, _a2: &str) -> String {
    "null".into()
}
