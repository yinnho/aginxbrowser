//! Touch-dispatch contract suite (#156, `Input.dispatchTouchEvent`).
//! A sibling file module (registered in cdp/mod.rs) rather than another
//! entry in dispatch.rs's inline `mod tests` so the god-file ratchet only
//! ever sees dispatch.rs shrink (scripts/audit_layers.py §G).

use crate::cdp::dispatch::{dispatch, CdpContext};
use crate::cdp::types::CdpRequest;
use serde_json::{json, Value};

fn create_page(ctx: &mut CdpContext) -> String {
    ctx.create_page_in_context(None)
        .expect("default browser context must exist")
}

async fn eval_in(ctx: &mut CdpContext, session_id: &str, id: u64, expression: &str) -> String {
    let req = CdpRequest {
        id,
        method: "Runtime.evaluate".to_string(),
        params: json!({ "expression": expression, "returnByValue": true }),
        session_id: Some(session_id.to_string()),
    };
    let resp = dispatch(&req, ctx).await;
    assert!(resp.error.is_none(), "evaluate failed: {:?}", resp.error);
    resp.result.expect("result")["result"]["value"]
        .as_str()
        .expect("string value")
        .to_string()
}

async fn touch(ctx: &mut CdpContext, session_id: &str, id: u64, kind: &str, points: Value) {
    let req = CdpRequest {
        id,
        method: "Input.dispatchTouchEvent".to_string(),
        params: json!({ "type": kind, "touchPoints": points }),
        session_id: Some(session_id.to_string()),
    };
    let resp = dispatch(&req, ctx).await;
    assert!(resp.error.is_none(), "dispatchTouchEvent failed: {:?}", resp.error);
}

/// The page every probe loads: a tappable button that records the event
/// chain, with per-family detail. Log entries are `name:detail` where the
/// detail carries what each test pins (pointerType/pointerId/isPrimary for
/// pointers, list lengths + identifiers for touch events).
const CHAIN_PAGE: &str = "data:text/html,<html><body><button id='b' style='width:200px;height:80px'>tap</button><script>\
window.__log = [];\
function rec(name, fn) { document.addEventListener(name, function(e) {\
  var d = '';\
  if (name.indexOf('pointer') === 0) d = e.pointerType + '/' + e.pointerId + '/' + e.isPrimary;\
  else if (name.indexOf('touch') === 0) d = e.touches.length + '-' + e.changedTouches.length +\
    ':' + Array.prototype.map.call(e.changedTouches, function(t) { return t.identifier; }).join(',');\
  __log.push(name + ':' + d);\
}, true); }\
['pointerdown','pointercancel','pointerup','touchstart','touchmove','touchend','touchcancel','mousedown','mouseup','click'].forEach(rec);\
window.__btn = 0;\
document.getElementById('b').addEventListener('click', function() { window.__btn++; });\
</script></body></html>";

async fn read_log(ctx: &mut CdpContext, session_id: &str, id: u64) -> Vec<String> {
    eval_in(ctx, session_id, id, "window.__log.join('|')")
        .await
        .split('|')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// The headline pin: a single-finger tap drives Chrome's whole observable
/// chain in order — pointerdown → touchstart → pointerup → touchend →
/// compat mousedown/mouseup → click — with the pointer typed 'touch', the
/// TouchEvent lists shaped right, and the constructors exposed. The bare
/// ack this batch replaced fired none of it while reporting success
/// (obscura#1086).
#[tokio::test(flavor = "current_thread")]
async fn touch_tap_fires_pointer_touch_and_click_chains() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-touch-tap".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": CHAIN_PAGE }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    // The constructors must exist as globals with real class semantics
    // before any dispatch (bootstrap.js exposes the Chrome face).
    let ctors = eval_in(
        &mut ctx,
        &session_id,
        2,
        "(function(){\
           var ok = typeof Touch === 'function' && typeof TouchEvent === 'function';\
           if (!ok) return 'missing';\
           var t = new Touch({identifier:7, target:document.body, clientX:5, clientY:6, radiusX:2, radiusY:3, rotationAngle:15, force:0.5});\
           var e = new TouchEvent('touchstart', {touches:[t], changedTouches:[t]});\
           return t.identifier + ',' + t.clientX + ',' + t.radiusX + ',' + t.rotationAngle + ',' + t.force\
             + ',' + (e instanceof Event) + ',' + e.touches.length + ',' + e.changedTouches[0].identifier;\
         })()",
    )
    .await;
    assert_eq!(ctors, "7,5,2,15,0.5,true,1,7", "Touch/TouchEvent faces: {ctors}");

    touch(
        &mut ctx,
        &session_id,
        3,
        "touchStart",
        json!([{ "x": 60.0, "y": 20.0 }]),
    )
    .await;
    touch(
        &mut ctx,
        &session_id,
        4,
        "touchEnd",
        json!([{ "x": 61.0, "y": 21.0 }]),
    )
    .await;

    let log = read_log(&mut ctx, &session_id, 5).await;
    assert_eq!(
        log,
        vec![
            // pointer family first, then the TouchEvent, per command.
            "pointerdown:touch/1/true",
            "touchstart:1-1:1",
            "pointerup:touch/1/true",
            // touchend: the released point is in changedTouches, NOT in
            // touches (nothing remains active).
            "touchend:0-1:1",
            // The tap's compat mouse events, then the trusted click.
            "mousedown:",
            "mouseup:",
            "click:",
        ],
        "tap chain: {log:?}"
    );

    // The first tap's click landed on the button itself.
    assert_eq!(
        eval_in(&mut ctx, &session_id, 6, "String(window.__btn)").await,
        "1",
        "the tap's click targets the button"
    );

    // A tap that lands outside the button still drives the touch chain,
    // but the button's own click counter must not move: the tap clicks
    // its hit target (body), and a click on body bubbles past the
    // button's non-capturing listener without hitting it.
    eval_in(&mut ctx, &session_id, 7, "window.__log = []; 'ok'").await;
    touch(
        &mut ctx,
        &session_id,
        8,
        "touchStart",
        json!([{ "x": 10.0, "y": 200.0 }]),
    )
    .await;
    touch(
        &mut ctx,
        &session_id,
        9,
        "touchEnd",
        json!([{ "x": 10.0, "y": 200.0 }]),
    )
    .await;
    let log = read_log(&mut ctx, &session_id, 10).await;
    assert!(
        log.iter().any(|e| e.starts_with("touchstart")),
        "the touch family still fires on body: {log:?}"
    );
    assert_eq!(
        eval_in(&mut ctx, &session_id, 11, "String(window.__btn)").await,
        "1",
        "a tap on empty body space must not click the button"
    );
}

/// touchMove carries the moved point in changedTouches while it stays in
/// touches, and a drag past the touch slop releases WITHOUT a click —
/// Chrome only clicks a tap, never a drag.
#[tokio::test(flavor = "current_thread")]
async fn touch_move_carries_changed_touches_and_drag_releases_without_click() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-touch-drag".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": CHAIN_PAGE }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    touch(&mut ctx, &session_id, 2, "touchStart", json!([{ "x": 30.0, "y": 20.0 }])).await;
    touch(&mut ctx, &session_id, 3, "touchMove", json!([{ "x": 90.0, "y": 50.0 }])).await;
    touch(&mut ctx, &session_id, 4, "touchEnd", json!([{ "x": 95.0, "y": 55.0 }])).await;

    let log = read_log(&mut ctx, &session_id, 5).await;
    assert!(
        log.iter().any(|e| e == "touchmove:1-1:1"),
        "touchmove: the moved point stays in touches and leads changedTouches: {log:?}"
    );
    // 65px of movement is far past the 8px slop — no compat click.
    assert!(
        !log.iter().any(|e| e.starts_with("click") || e.starts_with("mousedown")),
        "a drag past the slop must not click: {log:?}"
    );
    assert!(
        log.iter().any(|e| e == "touchend:0-1:1"),
        "the touch family completes: {log:?}"
    );

    // The move's coordinates reach the page: pin via a coordinates probe.
    let coords = eval_in(
        &mut ctx,
        &session_id,
        6,
        "(function(){\
           var seen = [];\
           document.addEventListener('touchmove', function(e) {\
             seen.push(e.changedTouches[0].clientX + ',' + e.changedTouches[0].clientY);\
           }, true);\
           window.__coords = seen;\
           return 'ok';\
         })()",
    )
    .await;
    assert_eq!(coords, "ok");
    touch(&mut ctx, &session_id, 7, "touchStart", json!([{ "x": 10.0, "y": 10.0 }])).await;
    touch(&mut ctx, &session_id, 8, "touchMove", json!([{ "x": 42.0, "y": 17.0 }])).await;
    let seen = eval_in(&mut ctx, &session_id, 9, "window.__coords.join(';')").await;
    assert_eq!(seen, "42,17", "move coordinates land: {seen}");
}

/// A two-finger gesture: both ids arrive in one touchstart, isPrimary
/// belongs to the smallest id only, and releasing one finger leaves it out
/// of touches but keeps the other. Releasing the second finger of a pinch
/// must NOT click — the multi latch kills the tap.
#[tokio::test(flavor = "current_thread")]
async fn two_finger_gesture_tracks_ids_and_pinch_release_does_not_click() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-touch-pinch".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": CHAIN_PAGE }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    touch(
        &mut ctx,
        &session_id,
        2,
        "touchStart",
        json!([{ "x": 40.0, "y": 20.0, "id": 1 }, { "x": 120.0, "y": 20.0, "id": 2 }]),
    )
    .await;
    touch(
        &mut ctx,
        &session_id,
        3,
        "touchEnd",
        json!([{ "x": 40.0, "y": 20.0, "id": 1 }]),
    )
    .await;

    let log = read_log(&mut ctx, &session_id, 4).await;
    assert_eq!(
        log.iter().filter(|e| e.starts_with("pointerdown")).count(),
        2,
        "both fingers get their pointerdown: {log:?}"
    );
    assert!(
        log.iter().any(|e| e == "pointerdown:touch/1/true")
            && log.iter().any(|e| e == "pointerdown:touch/2/false"),
        "isPrimary belongs to the smallest active id only: {log:?}"
    );
    assert!(
        log.iter().any(|e| e == "touchstart:2-2:1,2"),
        "one touchstart carries both ids in both lists: {log:?}"
    );
    assert!(
        log.iter().any(|e| e == "touchend:1-1:1"),
        "releasing finger 1 leaves finger 2 in touches: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e.starts_with("click")),
        "releasing one finger of a pinch must not click: {log:?}"
    );

    // Release the second finger too: still no click — the gesture was
    // multi-finger, the tap latch must stay dead for the whole gesture.
    eval_in(&mut ctx, &session_id, 5, "window.__log = []; 'ok'").await;
    touch(
        &mut ctx,
        &session_id,
        6,
        "touchEnd",
        json!([{ "x": 120.0, "y": 20.0, "id": 2 }]),
    )
    .await;
    let log = read_log(&mut ctx, &session_id, 7).await;
    assert_eq!(log, vec!["pointerup:touch/2/true", "touchend:0-1:2"],
        "last finger's full chain, primary now finger 2: {log:?}");
    assert!(
        !log.iter().any(|e| e.starts_with("click") || e.starts_with("mousedown")),
        "the last release of a pinch must not click either: {log:?}"
    );

    // Gesture over: the latch resets — a fresh single-finger tap clicks.
    eval_in(&mut ctx, &session_id, 8, "window.__log = []; 'ok'").await;
    touch(&mut ctx, &session_id, 9, "touchStart", json!([{ "x": 60.0, "y": 20.0 }])).await;
    touch(&mut ctx, &session_id, 10, "touchEnd", json!([{ "x": 60.0, "y": 20.0 }])).await;
    let log = read_log(&mut ctx, &session_id, 11).await;
    assert!(
        log.iter().any(|e| e.starts_with("click")),
        "a fresh single-finger gesture after the pinch taps normally: {log:?}"
    );
}

/// touchCancel drives pointercancel + touchcancel and clears the point
/// from the active set — a later gesture on the same id starts clean.
#[tokio::test(flavor = "current_thread")]
async fn touch_cancel_fires_pointercancel_and_clears_state() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-touch-cancel".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": CHAIN_PAGE }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    touch(&mut ctx, &session_id, 2, "touchStart", json!([{ "x": 60.0, "y": 20.0 }])).await;
    touch(&mut ctx, &session_id, 3, "touchCancel", json!([{ "x": 60.0, "y": 20.0 }])).await;

    let log = read_log(&mut ctx, &session_id, 4).await;
    assert!(
        log.iter().any(|e| e.starts_with("pointercancel:touch/1/true")),
        "pointercancel fires typed: {log:?}"
    );
    assert!(
        log.iter().any(|e| e == "touchcancel:0-1:1"),
        "touchcancel: canceled point in changedTouches, touches empty: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e.starts_with("click")),
        "cancel never clicks: {log:?}"
    );

    // State is clean: a fresh tap on the same default id works end to end.
    eval_in(&mut ctx, &session_id, 5, "window.__log = []; 'ok'").await;
    touch(&mut ctx, &session_id, 6, "touchStart", json!([{ "x": 60.0, "y": 20.0 }])).await;
    touch(&mut ctx, &session_id, 7, "touchEnd", json!([{ "x": 60.0, "y": 20.0 }])).await;
    let log = read_log(&mut ctx, &session_id, 8).await;
    assert!(
        log.iter().any(|e| e.starts_with("touchstart:1-1:1")) && log.iter().any(|e| e.starts_with("click")),
        "a post-cancel tap runs the full chain: {log:?}"
    );
}
