//! The firstPassUsable delivery gate, as a test.
//!
//! archify's ordinary-model-floor benchmark freezes attempt-1 outputs and
//! re-verifies them externally; ours does the same against the engine's own
//! contract: every corpus document must render on the first pass with zero
//! diagnostics and zero route repairs (repairs are disclosed, but a corpus
//! spec needing one is a readability regression), and the frozen sha256s
//! make every geometry change a conscious re-freeze rather than drift.

use sha2::{Digest, Sha256};

use super::{render, render_with_quality, render_with_theme, theme, checks::Quality};

const CHECKOUT: &str = r#"# Checkout

A two-lane pipeline with a dashed persistence edge.

```archify
{"workflow":{"title":"Checkout","lanes":[
  {"id":"web","label":"Web"},
  {"id":"svc","label":"Services"}],
 "nodes":[
  {"id":"cart","lane":"web","col":0,"label":"Cart","type":"frontend"},
  {"id":"pay","lane":"web","col":1,"label":"Pay","type":"frontend"},
  {"id":"orders","lane":"svc","col":2,"label":"Orders","type":"backend"},
  {"id":"db","lane":"svc","col":3,"label":"DB","type":"database"}],
 "edges":[
  {"from":"cart","to":"pay","label":"checkout"},
  {"from":"pay","to":"orders","label":"charge"},
  {"from":"orders","to":"db","label":"insert","variant":"dashed"}]}}
```
"#;

const INDEX_BUILD: &str = r#"```archify
{"workflow":{"title":"Index build","lanes":[
  {"id":"crawler","label":"Crawler"},
  {"id":"indexer","label":"Indexer"}],
 "nodes":[
  {"id":"seed","lane":"crawler","col":0,"label":"Seed list","type":"external"},
  {"id":"fetch","lane":"crawler","col":1,"label":"Fetch pages","sublabel":"tier1/tier3","type":"frontend"},
  {"id":"store","lane":"crawler","col":2,"label":"Raw store","type":"database","yOffset":-6},
  {"id":"parse","lane":"indexer","col":2,"label":"Parse","type":"backend","yOffset":6},
  {"id":"rank","lane":"indexer","col":3,"label":"Rank","type":"backend"},
  {"id":"serve","lane":"indexer","col":4,"label":"Serve","type":"cloud"}],
 "edges":[
  {"from":"seed","to":"fetch","label":"urls"},
  {"from":"fetch","to":"store","label":"html"},
  {"from":"store","to":"parse","label":"raw"},
  {"from":"parse","to":"rank","label":"terms"},
  {"from":"rank","to":"serve","label":"top-k"},
  {"from":"rank","to":"fetch","label":"recrawl","variant":"return"}]}}
```
"#;

const DOGFOOD: &str = r#"# Docgen render pipeline

```archify
{"workflow":{
  "title":"Render pipeline",
  "lanes":[
    {"id":"agent","label":"Agent"},
    {"id":"engine","label":"Engine"},
    {"id":"repair","label":"Repair","variant":"exception"}],
  "nodes":[
    {"id":"md","lane":"agent","col":0,"label":"Markdown","sublabel":"prose + fences","type":"external"},
    {"id":"parse","lane":"engine","col":1,"label":"Parse fence","sublabel":"typed JSON","type":"backend","tag":"deny_unknown"},
    {"id":"validate","lane":"engine","col":2,"label":"Validate","type":"backend"},
    {"id":"solve","lane":"engine","col":3,"label":"Solve layout","sublabel":"columns + routes","type":"backend","tag":"integer tenths"},
    {"id":"emit","lane":"engine","col":4,"label":"Emit SVG","type":"frontend"},
    {"id":"sha","lane":"agent","col":5,"label":"Receipt","sublabel":"sha256","type":"database"},
    {"id":"retry","lane":"repair","col":3,"label":"Widen gap","sublabel":"feedback loop","type":"security"}],
  "edges":[
    {"from":"md","to":"parse","label":"document"},
    {"from":"parse","to":"validate","label":"spec"},
    {"from":"validate","to":"solve","label":"clean"},
    {"from":"solve","to":"emit","label":"geometry"},
    {"from":"emit","to":"sha","label":"artifact"},
    {"from":"solve","to":"retry","label":"infeasible","variant":"security"},
    {"from":"retry","to":"solve","label":"re-solve","variant":"return","route":"drop"},
    {"from":"sha","to":"md","label":"receipt","variant":"dashed","route":"up-channel"}],
  "phases":[
    {"id":"p1","label":"Front half","fromCol":0,"toCol":2},
    {"id":"p2","label":"Back half","fromCol":3,"toCol":5}],
  "groups":[
    {"id":"g1","lane":"engine","label":"Deterministic core","fromCol":1,"toCol":4}]
}}
```
"#;

const CJK: &str = r#"```archify
{"workflow":{"title":"订单履约","lanes":[
  {"id":"shop","label":"店铺"},
  {"id":"wms","label":"仓配","variant":"exception"}],
 "nodes":[
  {"id":"order","lane":"shop","col":0,"label":"订单接收","sublabel":"分钟级下单","type":"frontend"},
  {"id":"risk","lane":"shop","col":1,"label":"风控校验","type":"security"},
  {"id":"split","lane":"wms","col":2,"label":"波次拆分","type":"backend"},
  {"id":"pick","lane":"wms","col":3,"label":"拣货打包","sublabel":"边拣边分","type":"backend"},
  {"id":"ship","lane":"wms","col":4,"label":"干线发运","type":"cloud"}],
 "edges":[
  {"from":"order","to":"risk","label":"下单"},
  {"from":"risk","to":"split","label":"放行"},
  {"from":"split","to":"pick","label":"波次"},
  {"from":"pick","to":"ship","label":"包裹"},
  {"from":"ship","to":"order","label":"回传单号","variant":"return"}]}}
```
"#;

const SEQ_FIVE: &str = r#"# Cache miss

The five-party read path with all four message variants.

```archify
{"sequence":{"title":"缓存击穿","participants":[
  {"id":"user","type":"external","label":"用户"},
  {"id":"api","type":"backend","label":"API 网关"},
  {"id":"svc","type":"backend","label":"查询服务"},
  {"id":"redis","type":"database","label":"Redis","sublabel":"缓存"},
  {"id":"mysql","type":"database","label":"MySQL"}],
 "messages":[
  {"from":"user","to":"api","label":"GET /profile","variant":"emphasis"},
  {"from":"api","to":"svc","label":"query"},
  {"from":"svc","to":"redis","label":"hgetall"},
  {"from":"redis","to":"svc","label":"nil","variant":"return"},
  {"from":"svc","to":"mysql","label":"select ..."},
  {"from":"mysql","to":"svc","label":"rows","variant":"return"},
  {"from":"svc","to":"redis","label":"setex 300"},
  {"from":"svc","to":"api","label":"profile","variant":"dashed"},
  {"from":"api","to":"user","label":"200 OK"}]}}
```
"#;

const SEQ_MIN: &str = r#"```archify
{"sequence":{"title":"Ping","participants":[
  {"id":"a","type":"frontend","label":"Client"},
  {"id":"b","type":"backend","label":"Server"}],
 "messages":[{"from":"a","to":"b","label":"ping"}]}}
```
"#;

const DATAFLOW_INGEST: &str = r#"# Search ingest

Three stages with a dashed skip edge around the middle stage.

```archify
{"dataflow":{"title":"Search ingest","stages":[
  {"label":"Collect"},{"label":"Index"},{"label":"Serve"}],
 "nodes":[
  {"id":"crawl","type":"frontend","label":"Crawler","stage":0,"row":0,"tag":"tier1"},
  {"id":"store","type":"database","label":"Raw store","stage":1,"row":0,"sublabel":"objects"},
  {"id":"index","type":"backend","label":"Indexer","stage":1,"row":2},
  {"id":"query","type":"cloud","label":"Query API","stage":2,"row":2}],
 "flows":[
  {"from":"crawl","to":"index","label":"skip","variant":"dashed"},
  {"from":"crawl","to":"store","label":"pages"},
  {"from":"index","to":"query","label":"shards"},
  {"from":"store","to":"index","label":"docs"}]}}
```
"#;

const LIFECYCLE_RELEASE: &str = r#"# Release

The rollout across all three bands, with an on-call detour and a retry loop.

```archify
{"lifecycle":{"title":"Release","lanes":[
  {"id":"main","label":"Rollout"},
  {"id":"ops","label":"On-call"},
  {"id":"terminal","label":"End states"}],
 "states":[
  {"id":"build","type":"start","label":"Build","lane":"main","col":0,"step":"01"},
  {"id":"stage","type":"active","label":"Staging","lane":"main","col":1},
  {"id":"alarm","type":"waiting","label":"Alert","lane":"ops","col":1,"sublabel":"page"},
  {"id":"rollback","type":"failure","label":"Rollback","lane":"ops","col":2},
  {"id":"live","type":"success","label":"Live","lane":"terminal","col":1}],
 "transitions":[
  {"from":"alarm","to":"rollback","label":"auto"},
  {"from":"build","to":"stage","label":"promote"},
  {"from":"rollback","to":"build","label":"retry","variant":"dashed"},
  {"from":"stage","to":"alarm","label":"errors","variant":"security"},
  {"from":"stage","to":"live","label":"pass"}]}}
```
"#;

const ARCHITECTURE_EDGE: &str = r#"```archify
{"architecture":{"title":"Edge serving","components":[
  {"id":"dns","type":"cloud","label":"DNS","row":0,"col":0},
  {"id":"lb","type":"cloud","label":"Load balancer","row":0,"col":1},
  {"id":"web","type":"frontend","label":"Web","row":1,"col":1},
  {"id":"api","type":"backend","label":"API","row":1,"col":2},
  {"id":"db","type":"database","label":"Primary DB","row":2,"col":2}],
 "boundaries":[
  {"kind":"security-group","label":"Private subnet","wraps":["api","db"]}],
 "connections":[
  {"from":"api","to":"db","label":"sql"},
  {"from":"dns","to":"lb","label":"resolve"},
  {"from":"lb","to":"web","label":"http"},
  {"from":"web","to":"api","label":"json"},
  {"from":"web","to":"db","label":"cache miss","variant":"dashed"}]}}
```
"#;

/// (name, markdown) — the corpus. Attempt-1 for every entry must be clean:
/// no diagnostics, no repairs, bytes frozen.
const CORPUS: [(&str, &str); 9] = [
    ("checkout", CHECKOUT),
    ("index-build", INDEX_BUILD),
    ("dogfood", DOGFOOD),
    ("cjk", CJK),
    ("seq-five", SEQ_FIVE),
    ("seq-min", SEQ_MIN),
    ("dataflow-ingest", DATAFLOW_INGEST),
    ("lifecycle-release", LIFECYCLE_RELEASE),
    ("architecture-edge", ARCHITECTURE_EDGE),
];

/// Frozen attempt-1 sha256s (light theme). A mismatch means geometry or
/// palette changed; if that change is deliberate, re-freeze this table
/// consciously — the point of the gate is that "the numbers moved" is never
/// a surprise. Last re-freeze: 批6b preset+sigil layer (every node gains a
/// semantic sigil stamp and the html tag gains data-preset; classic values
/// themselves are unchanged).
const FROZEN: [(&str, &str); 9] = [
    ("checkout", "4b3fb05e81d5d82b9700d61b3a654a8f44b2b83ce4f41c51d83ab26cc05f79fa"),
    ("index-build", "f2e66a7c43e96955cbf7d426f3bafde95e98339ed1d234127cd4d9a0f6c66c83"),
    ("dogfood", "697db655881704f9e0a82c93fa5a46d706c795ad8e5178e27f49542a33694729"),
    ("cjk", "48ef00bb6cac9a862ce889193d7248fceb54aaa214ce1c8d4a86195ceff6b6d5"),
    ("seq-five", "9a5e6be0d16854fac1cfecfee5a6186ed424148fabe5a7e034dadf339216fff1"),
    ("seq-min", "9350c4342b49b8cda009fa82af7fc53b13da91655053e5f2baca64b246fe244d"),
    ("dataflow-ingest", "308c185190d3f945822c89b3a6e7194bb284c6071ffaae72ba10f53a5801d117"),
    ("lifecycle-release", "28022abeb45469eb657de784f0f903fbea0080029596fc5dbbe43f7a8e307326"),
    ("architecture-edge", "ac8f3cb36d3254378197330640e410bf7053ec7adf39cf4aaaa73e4b04c5aa6d"),
];

/// Dark-theme corpus: the two richest documents re-rendered under DARK, so
/// the remap is frozen too — a light-only gate would let the dark palette
/// drift invisibly. Same corpus markdown, same zero-diagnostic contract.
const DARK_CORPUS: [&str; 2] = [DOGFOOD, SEQ_FIVE];

const FROZEN_DARK: [(&str, &str); 2] = [
    ("dogfood", "892f92ea1fde213f2ef7a9904bb1e3a6c0b1bc823d82b6f0c8b4089bf2537c06"),
    ("seq-five", "dd20d3f19f20cf7bb5099b898b260a9b086ae51b0cf055ce9818b28354bd7779"),
];

/// Preset corpus: one document under each non-classic palette family, in a
/// different mode, paired with its same-mode classic baseline. The preset
/// tables are generator output — this freeze is what makes a typo in those
/// tables a conscious re-freeze instead of silent drift.
const PRESET_CORPUS: [(&str, &str, &theme::Theme, &theme::Theme); 2] = [
    ("dogfood/signal-flow-dark", DOGFOOD, &theme::SIGNAL_FLOW_DARK, &theme::DARK),
    ("seq-five/blueprint-light", SEQ_FIVE, &theme::BLUEPRINT_LIGHT, &theme::LIGHT),
];

const FROZEN_PRESET: [(&str, &str); 2] = [
    ("dogfood/signal-flow-dark", "d7084469d16f41284e4bb3272a5c65754ca1145c54f936a2583b3c09c671d672"),
    ("seq-five/blueprint-light", "c84125deac8818d688ec029eeea869873a186065e85a00b113d338cd9f1e6130"),
];

#[test]
fn first_pass_corpus_is_clean() {
    for (name, md) in CORPUS {
        let outcome = render(md);
        let diagnostics = outcome.receipt["diagnostics"].as_array().unwrap();
        assert!(diagnostics.is_empty(), "{name}: {diagnostics:?}");
        let repaired: usize = outcome.receipt["diagrams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                d.get("repairs")
                    .and_then(|r| r.as_array())
                    .map_or(0, |a| a.len())
            })
            .sum();
        assert_eq!(repaired, 0, "{name}: attempt-1 must not need repairs");
    }
}

/// The showcase bar on the corpus: the same attempt-1 documents, graded at
/// the delivery profile, must report zero findings. A corpus document
/// failing showcase is a composition regression (or an over-strict check)
/// — never a quiet pass.
#[test]
fn corpus_passes_the_showcase_audit() {
    for (name, md) in CORPUS {
        let outcome = render_with_quality(md, &theme::LIGHT, Quality::Showcase);
        for d in outcome.receipt["diagrams"].as_array().unwrap() {
            let c = &d["composition"];
            assert_eq!(
                c["status"], "pass",
                "{name} diagram {}: {:?}",
                d["index"], c["issues"]
            );
            assert!(
                c["issues"].as_array().unwrap().is_empty(),
                "{name} diagram {}: {:?}",
                d["index"],
                c["issues"]
            );
        }
        // And the receipt's aggregate line agrees.
        assert!(
            outcome.receipt["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap_or("").contains("composition audit (showcase): clean")),
            "{name}: aggregate audit line missing: {:?}",
            outcome.receipt["checks"]
        );
    }
}

/// The quality axis is report-only: standard and showcase render the exact
/// same artifact bytes, and only the receipt differs. This is what lets the
/// delivery gate flip without invalidating every cached artifact.
#[test]
fn quality_profiles_do_not_change_the_artifact() {
    for (name, md) in CORPUS {
        let standard = render(md);
        let showcase = render_with_quality(md, &theme::LIGHT, Quality::Showcase);
        assert_eq!(
            standard.html, showcase.html,
            "{name}: showcase must not touch the bytes"
        );
        assert_eq!(standard.receipt["sha256"], showcase.receipt["sha256"]);
        assert_eq!(standard.receipt["quality"], "standard");
        assert_eq!(showcase.receipt["quality"], "showcase");
    }
}

#[test]
fn first_pass_corpus_bytes_are_frozen() {
    let computed: Vec<(&str, String)> = CORPUS
        .iter()
        .map(|&(name, md)| {
            let outcome = render(md);
            let mut h = Sha256::new();
            h.update(outcome.html.as_bytes());
            (name, format!("{:x}", h.finalize()))
        })
        .collect();
    assert_eq!(computed.len(), FROZEN.len());
    let drift: Vec<String> = computed
        .iter()
        .zip(FROZEN.iter())
        .filter(|((_, hash), &(_, frozen))| hash.as_str() != frozen)
        .map(|((name, hash), _)| format!("(\"{name}\", \"{hash}\"),"))
        .collect();
    assert!(
        drift.is_empty(),
        "corpus bytes drifted — pin these: {}",
        drift.join(" ")
    );
}

/// Same freeze discipline for the dark palette, and the two themes must
/// never alias: a dark render equal to its light render means the theme
/// stopped flowing into the artifact. All drift collects into one panic so
/// a re-freeze pins every entry in a single run.
#[test]
fn dark_corpus_bytes_are_frozen_and_distinct() {
    let names = ["dogfood", "seq-five"];
    let computed: Vec<(&str, String)> = DARK_CORPUS
        .iter()
        .enumerate()
        .map(|(i, &md)| {
            let light = render(md).html;
            let dark = render_with_theme(md, &theme::DARK).html;
            assert_ne!(
                light, dark,
                "{}: dark render must differ from light",
                names[i]
            );
            assert!(
                dark.contains("data-theme=\"dark\""),
                "{}: dark provenance attribute missing",
                names[i]
            );
            let mut h = Sha256::new();
            h.update(dark.as_bytes());
            (names[i], format!("{:x}", h.finalize()))
        })
        .collect();
    let drift: Vec<String> = computed
        .iter()
        .zip(FROZEN_DARK.iter())
        .filter(|((_, hash), &(_, frozen))| hash.as_str() != frozen)
        .map(|((name, hash), _)| format!("(\"{name}\", \"{hash}\"),"))
        .collect();
    assert!(
        drift.is_empty(),
        "dark corpus bytes drifted — pin these: {}",
        drift.join(" ")
    );
}

/// Freeze discipline for the preset families, plus their anti-aliasing
/// contract: a preset render equal to its same-mode classic render means
/// the preset palette stopped flowing into the artifact.
#[test]
fn preset_corpus_bytes_are_frozen_and_distinct() {
    let computed: Vec<(&str, String, String)> = PRESET_CORPUS
        .iter()
        .map(|&(key, md, preset, _)| {
            let rendered = render_with_theme(md, preset);
            let mut h = Sha256::new();
            h.update(rendered.html.as_bytes());
            (key, rendered.html, format!("{:x}", h.finalize()))
        })
        .collect();
    let drift: Vec<String> = computed
        .iter()
        .zip(FROZEN_PRESET.iter())
        .filter(|((_, _, hash), &(_, frozen))| hash.as_str() != frozen)
        .map(|((key, _, hash), _)| format!("(\"{key}\", \"{hash}\"),"))
        .collect();
    assert!(
        drift.is_empty(),
        "preset corpus bytes drifted — pin these: {}",
        drift.join(" ")
    );
    // Wiring, checked once the freeze holds: the preset reaches the shell
    // as provenance and the palette stays distinct from classic in-mode.
    for (i, &(key, md, preset, classic)) in PRESET_CORPUS.iter().enumerate() {
        let html = &computed[i].1;
        assert!(
            html.contains(&format!("data-preset=\"{}\"", preset.preset)),
            "{key}: preset provenance attribute missing"
        );
        assert_ne!(
            html.as_str(),
            render_with_theme(md, classic).html.as_str(),
            "{key}: preset render must differ from its classic baseline"
        );
    }
}
