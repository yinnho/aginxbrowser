//! Named document-start recipes (opt-in) — no per-host auto-mount.
//!
//! Platform policy (signing wrappers, logout fuses, site quirks) lives in
//! `workflow/<name>/`, not in the engine's default navigation path.
//! Recipe **bodies** live as `workflow/<recipe>/recipe.js` and are
//! `include_str!`'d here so edit/review stays next to flows; rebuild
//! picks them up. Resolve/aliases stay in this module. The
//! `xhs-sign` recipe remains callable via `POST /session/:id/preload`
//! `{"recipe":"xhs-sign"}` (or alias `"xhs"`) when a caller or flow wants
//! the maintained document-start wrapper without pasting the source —
//! but matching `*.xiaohongshu.com` / `*.xhslink.com` no longer mounts it
//! automatically (demoted 2026-10-12: delivery unit = engine + flow;
//! earlier auto-mount was #84 / batch 286, reversed to match the
//! BUILTIN_FLOWS revoke of 2026-10-02).
//!
//! Lineage of the xhs script body: the former eval-time mega patch from
//! `workflow/xhs-post/flow.json`, reshaped for the www read path
//! (swallow page x-s/x-t and re-sign — page VM signatures computed
//! in-engine come out wrong; CDP bridge measured comment/page 461→200).
//! Document-start wins before inline jsvmp captures fetch/XHR natives.
//! Flows must `preload` `recipe: "xhs-sign"` (no host auto-mount, no
//! duplicate eval mega-patch). Logout fuse stays (unsigned-API 406s
//! misread as session death trigger the page's own /sso/logout).
//!
//! Also named here (opt-in only): `taobao-block-everyhelp` — fetch/XHR
//! reject for `everyhelp.(cdn.)taobao.com` (2026-10-06/07 wall). Shared
//! by taobao-* / tmall-* flows so the script is not pasted eight times.
//! `taobao-block-qn-header` blocks qn_header / microapp-loader / workbench
//! script injection on publish pages (four publish flows share one body).

/// Canonical recipe name for the xhs signing wrapper.
pub const XHS_SIGN_NAME: &str = "xhs-sign";

/// The xhs document-start signing wrapper (#84, batch 286).
///
/// Opt-in only — see module docs. Observability on `window`:
/// `__xs_installed` / `__xs_swallowed` / `__xs_signed` / `__xs_passthrough` / `__xs_fuse`.
pub const XHS_SIGN: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/xhs-sign/recipe.js"));


/// Canonical recipe name for the taobao everyhelp fetch/XHR wall.
pub const TAOBAO_BLOCK_EVERYHELP_NAME: &str = "taobao-block-everyhelp";

/// Block `everyhelp.(cdn.)taobao.com` at fetch/XHR open (2026-10-06/07).
///
/// Opt-in via `preload` `{"recipe":"taobao-block-everyhelp"}` (alias
/// `"taobao-everyhelp"`). The widget's getWidgetVersion against everyhelp
/// was a hung-fetch wall; rejecting at the transport face keeps login /
/// shop / publish pages moving.
pub const TAOBAO_BLOCK_EVERYHELP: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/taobao-block-everyhelp/recipe.js"));


/// Canonical recipe name for the taobao/tmall qn micro-mod script wall.
pub const TAOBAO_BLOCK_QN_HEADER_NAME: &str = "taobao-block-qn-header";

/// Block qn_header / crs-qn microapp-loader / qnWorkbench / ihelper sdk-loader
/// `<script>` injection (publish stable-boot recipe). Opt-in via
/// `{"recipe":"taobao-block-qn-header"}` (alias `"taobao-qn-header"`).
pub const TAOBAO_BLOCK_QN_HEADER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/taobao-block-qn-header/recipe.js"));

/// Resolve a recipe name (aliases included) to its source. None for
/// unknown names — callers surface the error rather than silently
/// mounting nothing.
pub fn resolve(name: &str) -> Option<&'static str> {
    match name {
        XHS_SIGN_NAME | "xhs" => Some(XHS_SIGN),
        TAOBAO_BLOCK_EVERYHELP_NAME | "taobao-everyhelp" => Some(TAOBAO_BLOCK_EVERYHELP),
        TAOBAO_BLOCK_QN_HEADER_NAME | "taobao-qn-header" => Some(TAOBAO_BLOCK_QN_HEADER),
        _ => None,
    }
}

/// Builtin recipe scripts that apply to this navigation URL.
///
/// Always empty: site recipes are not auto-mounted. Kept so navigate /
/// create keep calling one function (`effective_scripts`) without a
/// special case; a future opt-in host table would land here behind an
/// explicit product decision, not by default.
fn builtin_scripts(_url: &str) -> Vec<&'static str> {
    Vec::new()
}

/// Report-only view of [`builtin_scripts`] — the names surfaces quote
/// (navigate responses) without the source body. Always empty while
/// auto-mount is off.
pub fn builtin_names(url: &str) -> Vec<&'static str> {
    let _ = url;
    Vec::new()
}

/// The document-start group for a navigation: any future builtin
/// recipes would ride first; today this is just the user group.
pub fn effective_scripts(url: &str, user: &[String]) -> Vec<String> {
    builtin_scripts(url)
        .into_iter()
        .map(str::to_string)
        .chain(user.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_host_auto_mount() {
        for url in [
            "https://www.xiaohongshu.com/explore?channel_id=homefeed_recommend",
            "https://xiaohongshu.com/",
            "http://edith.xiaohongshu.com/api/sns/web/v1/feed",
            "https://xhslink.com/aBc123",
            "https://example.com/",
            "about:blank",
        ] {
            assert!(
                builtin_names(url).is_empty(),
                "auto-mount must stay off: {url}"
            );
            assert_eq!(
                effective_scripts(url, &[]),
                Vec::<String>::new(),
                "empty user group stays empty: {url}"
            );
        }
    }

    #[test]
    fn effective_passes_user_group_unchanged() {
        let user = vec!["window.__mine = 1;".to_string()];
        assert_eq!(
            effective_scripts("https://www.xiaohongshu.com/explore", &user),
            user
        );
        assert_eq!(effective_scripts("https://example.com/", &user), user);
    }

    #[test]
    fn resolve_known_names_and_rejects_unknown() {
        assert!(resolve("xhs-sign").is_some());
        assert!(resolve("xhs").is_some());
        assert!(resolve("taobao-block-everyhelp").is_some());
        assert!(resolve("taobao-everyhelp").is_some());
        assert!(resolve("taobao-block-qn-header").is_some());
        assert!(resolve("taobao-qn-header").is_some());
        assert!(resolve("nope").is_none());
    }

    #[test]
    fn taobao_everyhelp_script_carries_the_contract() {
        for needle in ["everyhelp", "XMLHttpRequest.prototype.open", "window.fetch", "__ehb"] {
            assert!(
                TAOBAO_BLOCK_EVERYHELP.contains(needle),
                "script lost: {needle}"
            );
        }
    }

    #[test]
    fn script_carries_the_contract() {
        // Guards against silent truncation of the embedded source: every
        // face the wrapper must own is named.
        for needle in [
            "__xs_installed",
            "_webmsxyw",
            "sso\\/logout",
            "XMLHttpRequest.prototype.send",
            "window.fetch",
            "__xs_swallowed",
            "__xs_passthrough",
        ] {
            assert!(XHS_SIGN.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn taobao_qn_header_script_carries_the_contract() {
        for needle in ["qn_header", "microapp-loader", "appendChild", "BLOCK"] {
            assert!(
                TAOBAO_BLOCK_QN_HEADER.contains(needle),
                "script lost: {needle}"
            );
        }
    }
}

