//! Timezone-override contract suite. A sibling file module (registered in
//! cdp/mod.rs) rather than another entry in dispatch.rs's inline `mod tests`
//! so the god-file ratchet only ever sees dispatch.rs shrink
//! (scripts/audit_layers.py §G).

use crate::cdp::dispatch::{dispatch, CdpContext};
use crate::cdp::types::CdpRequest;
use serde_json::json;

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

#[tokio::test(flavor = "current_thread")]
async fn timezone_override_moves_intl_and_date_and_survives_navigation() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-tz".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = |id| CdpRequest {
        id,
        method: "Page.navigate".to_string(),
        params: json!({ "url": "data:text/html,<html><body>hi</body></html>" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav(1), &mut ctx).await.error.is_none());

    let expr = "Intl.DateTimeFormat().resolvedOptions().timeZone + '|' + new Date('2026-01-15T12:00:00Z').getTimezoneOffset()";
    let set = CdpRequest {
        id: 2,
        method: "Emulation.setTimezoneOverride".to_string(),
        params: json!({ "timezoneId": "America/New_York" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 3, expr).await,
        "America/New_York|300"
    );

    assert!(dispatch(&nav(4), &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 5, expr).await,
        "America/New_York|300",
        "timezone pin must survive a navigation"
    );

    let bad = CdpRequest {
        id: 6,
        method: "Emulation.setTimezoneOverride".to_string(),
        params: json!({ "timezoneId": "Not/AZone" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&bad, &mut ctx).await.error.is_some());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 7, expr).await,
        "America/New_York|300",
        "a rejected timezone must leave the previous pin"
    );

    let clear = CdpRequest {
        id: 8,
        method: "Emulation.setTimezoneOverride".to_string(),
        params: json!({ "timezoneId": "" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&clear, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 9, expr).await,
        "Asia/Shanghai|-480"
    );
}
