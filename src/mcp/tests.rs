//! The mcp module's tests, split from the module root (ARCHITECTURE.md
//! P2 god-file ratchet).
use super::*;

#[test]
fn viewport_tiers_classify_the_readback_strategy() {
    let probe = |iw: i64, ih: i64, sw: i64, sh: i64| {
        grade_viewport(&json!({
            "innerWidth": iw, "innerHeight": ih,
            "scrollWidth": sw, "scrollHeight": sh,
            "diagrams": 1, "minScale": 0.25,
        }))
    };
    assert_eq!(probe(1280, 1000, 1280, 900)["tier"], "fits");
    assert_eq!(probe(1280, 1000, 1280, 2452)["tier"], "tall");
    assert_eq!(probe(1280, 1000, 2200, 900)["tier"], "wide");
    assert_eq!(probe(1280, 1000, 2200, 2452)["tier"], "oversized");
    // Facts pass through for the agent to reason with.
    let graded = probe(1280, 1000, 1280, 2452);
    assert_eq!(graded["minScale"], json!(0.25));
    assert_eq!(graded["diagrams"], json!(1));
    // Missing numbers degrade to zeros, not a panic.
    assert_eq!(grade_viewport(&json!({}))["tier"], "fits");
}
