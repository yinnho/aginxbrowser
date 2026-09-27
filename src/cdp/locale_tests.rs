//! Locale-override contract suite (#153, `Emulation.setLocaleOverride`).
//! A sibling file module (registered in cdp/mod.rs) rather than another
//! entry in dispatch.rs's inline `mod tests` so the god-file ratchet only
//! ever sees dispatch.rs shrink (scripts/audit_layers.py §G).

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
async fn locale_override_moves_navigator_intl_tz_and_survives_navigation() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-locale".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = |id| CdpRequest {
        id,
        method: "Page.navigate".to_string(),
        params: json!({ "url": "data:text/html,<html><body>hi</body></html>" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav(1), &mut ctx).await.error.is_none());

    // The three observable faces the pin owns: navigator.language(s), the
    // Intl default locale, and the language-derived default timezone (the
    // persona is zh-CN ⇒ Asia/Shanghai; a ja-JP pin must derive Tokyo).
    let expr = "navigator.language + '|' + navigator.languages.join(',') + '|' + \
                Intl.DateTimeFormat().resolvedOptions().locale + '|' + \
                Intl.DateTimeFormat().resolvedOptions().timeZone";
    let set = CdpRequest {
        id: 2,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "ja-JP" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 3, expr).await,
        "ja-JP|ja-JP,ja|ja-JP|Asia/Tokyo"
    );

    assert!(dispatch(&nav(4), &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 5, expr).await,
        "ja-JP|ja-JP,ja|ja-JP|Asia/Tokyo",
        "locale pin must survive a navigation"
    );

    let bad = CdpRequest {
        id: 6,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "not a locale!" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&bad, &mut ctx).await.error.is_some());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 7, expr).await,
        "ja-JP|ja-JP,ja|ja-JP|Asia/Tokyo",
        "a rejected locale must leave the previous pin"
    );

    let clear = CdpRequest {
        id: 8,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&clear, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 9, expr).await,
        "zh-CN|zh-CN,zh,en|zh-CN|Asia/Shanghai",
        "clearing must restore the persona language, not a blank"
    );
}

/// The pin outranks setUserAgentOverride's acceptLanguage — Chrome's locale
/// override wins over the UA metadata — and survives a later UA override
/// that moves the persona underneath it.
#[tokio::test(flavor = "current_thread")]
async fn locale_override_outranks_user_agent_accept_language() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-locale-ua".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": "data:text/html,<html><body>hi</body></html>" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    let set_locale = CdpRequest {
        id: 2,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "fr-FR" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set_locale, &mut ctx).await.error.is_none());
    let set_ua = CdpRequest {
        id: 3,
        method: "Emulation.setUserAgentOverride".to_string(),
        params: json!({ "userAgent": "TestUA", "acceptLanguage": "de-DE" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set_ua, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 4, "navigator.language").await,
        "fr-FR",
        "the locale pin must survive a UA override moving the acceptLanguage"
    );
    // ... but a later locale clear falls back to the UA's acceptLanguage,
    // exactly like Chrome's precedence chain.
    let clear = CdpRequest {
        id: 5,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&clear, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(&mut ctx, &session_id, 6, "navigator.language").await,
        "de-DE"
    );
}

/// An explicit timezone pin outranks the locale's derived zone — the
/// two overrides stay independent knobs, like Chrome's.
#[tokio::test(flavor = "current_thread")]
async fn timezone_pin_outranks_locale_derived_zone() {
    let mut ctx = CdpContext::new_with_options(None, false);
    let page_id = create_page(&mut ctx);
    let session_id = "sess-locale-tz".to_string();
    ctx.sessions.insert(session_id.clone(), page_id);
    let nav = CdpRequest {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({ "url": "data:text/html,<html><body>hi</body></html>" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&nav, &mut ctx).await.error.is_none());

    let set_tz = CdpRequest {
        id: 2,
        method: "Emulation.setTimezoneOverride".to_string(),
        params: json!({ "timezoneId": "America/New_York" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set_tz, &mut ctx).await.error.is_none());
    let set_locale = CdpRequest {
        id: 3,
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": "ja-JP" }),
        session_id: Some(session_id.clone()),
    };
    assert!(dispatch(&set_locale, &mut ctx).await.error.is_none());
    assert_eq!(
        eval_in(
            &mut ctx,
            &session_id,
            4,
            "Intl.DateTimeFormat().resolvedOptions().timeZone"
        )
        .await,
        "America/New_York",
        "an explicit timezone pin must not be re-derived from the locale"
    );
}
