//! The firstPassUsable delivery gate, as a test.
//!
//! archify's ordinary-model-floor benchmark freezes attempt-1 outputs and
//! re-verifies them externally; ours does the same against the engine's own
//! contract: every corpus document must render on the first pass with zero
//! diagnostics and zero route repairs (repairs are disclosed, but a corpus
//! spec needing one is a readability regression), and the frozen sha256s
//! make every geometry change a conscious re-freeze rather than drift.

use sha2::{Digest, Sha256};

use super::render;

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

/// (name, markdown) — the corpus. Attempt-1 for every entry must be clean:
/// no diagnostics, no repairs, bytes frozen.
const CORPUS: [(&str, &str); 6] = [
    ("checkout", CHECKOUT),
    ("index-build", INDEX_BUILD),
    ("dogfood", DOGFOOD),
    ("cjk", CJK),
    ("seq-five", SEQ_FIVE),
    ("seq-min", SEQ_MIN),
];

/// Frozen attempt-1 sha256s. A mismatch means geometry changed; if that
/// change is deliberate, re-freeze this table consciously — the point of
/// the gate is that "the numbers moved" is never a surprise.
const FROZEN: [(&str, &str); 6] = [
    ("checkout", "641633976ca6f4cd3a624991adb64277eaa1976fbc267dbcc17cf5cff80ecf60"),
    ("index-build", "ca9071e0c063da0b47a9264bcf02cfde82fec351343d84ea8daa04bc9cbeefd3"),
    ("dogfood", "dd8a9e43d42f99bfe64568cf80fb235025d6e542c01b0344086d09abb947fe77"),
    ("cjk", "4176a2822cb2e8dc3330a7e4666d3fbd5634f1d6003696265a5ae30ef92165a8"),
    ("seq-five", "99f0a486dad001b112272a137141565b2fb32c7bbf3194b2ee7335b5a3a60dcb"),
    ("seq-min", "ff1cac525bbf7fc5a71ebbdf753cf928f6a657dd644c4d21fe7f24887bfe3c33"),
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
