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
//! `xhs-qrcode` is the vendored QR lib for `xhs-login` (no inline in flow.json).
//! `x-p256` is the shared X.com P-256 helper for write flows.
//! `x-bearer` is the shared X.com bearer probe (localStorage cache / main.js scrape).
//! `x-auth-ctx` is the shared X.com txid ctx + session-bind helpers for write flows.

/// Canonical recipe name for the xhs signing wrapper.
pub const XHS_SIGN_NAME: &str = "xhs-sign";

/// The xhs document-start signing wrapper (#84, batch 286).
///
/// Opt-in only — see module docs. Observability: ONE non-enumerable
/// `window.__xsStats()` returning the counter snapshot (`installed` /
/// `swallowed` / `signed` / `passthrough` / `fuse` / …). No enumerable
/// `__xs_*` globals and no unmasked natives — wrappers go through the
/// engine's `_markNative` registry so 300015-class env probes reading
/// `Function.prototype.toString` / `Object.keys(window)` see Chrome.
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

/// Canonical recipe name for the xhs QR SVG library (qrcode-generator).
pub const XHS_QRCODE_NAME: &str = "xhs-qrcode";

/// Vendored qrcode-generator used by `workflow/xhs-login` to paint a scannable
/// SVG without inlining ~20KB into `flow.json`.
pub const XHS_QRCODE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/xhs-qrcode/recipe.js"));


/// Canonical recipe name for the shared X.com in-page P-256 helper.
pub const X_P256_NAME: &str = "x-p256";

/// BigInt / Jacobian P-256 for X write flows (`x-reply` / `x-follow`).
/// Installs `globalThis.P256`. Prefer flow `preload` with `apply_now` when
/// attaching to an already-navigated session.
pub const X_P256: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/x-p256/recipe.js"));

/// Canonical recipe name for the shared X.com bearer probe.
pub const X_BEARER_NAME: &str = "x-bearer";

/// Refresh `localStorage.abx_bearer` from live `main.*.js` when >12h old.
/// Installs `globalThis.abxProbeBearer`. Shared by x-reply / x-follow /
/// x-notifs / x-search / x-read. Prefer `apply_now` when attaching to a
/// live session.
pub const X_BEARER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/x-bearer/recipe.js"));

/// Canonical recipe name for the shared X.com txid ctx + session bind helpers.
pub const X_AUTH_CTX_NAME: &str = "x-auth-ctx";

/// Txid context + CreateWebSessionBind helpers for X write flows.
///
/// Installs `globalThis.abxEnsureAuthCtx` plus `abxB64keep` / `abxDerEncode` /
/// `abxAuthHeaders` / `abxTxidGen`. Requires `x-p256`. Shared by `x-reply` /
/// `x-follow`. Prefer `apply_now` when attaching to a live session.
pub const X_AUTH_CTX: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/workflow/x-auth-ctx/recipe.js"));

/// Resolve a recipe name (aliases included) to its source. None for
/// unknown names — callers surface the error rather than silently
/// mounting nothing.
pub fn resolve(name: &str) -> Option<&'static str> {
    match name {
        XHS_SIGN_NAME | "xhs" => Some(XHS_SIGN),
        XHS_QRCODE_NAME => Some(XHS_QRCODE),
        X_P256_NAME => Some(X_P256),
        X_BEARER_NAME => Some(X_BEARER),
        X_AUTH_CTX_NAME => Some(X_AUTH_CTX),
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

/// Recipes that [`crate::account::verify`] / [`crate::account::login`] probes
/// MUST mount before navigating `url`, so the probe cannot silently tear the
/// shared jar.
///
/// Distinct from [`builtin_names`]: general navigate stays recipe-free (the
/// 2026-10-12 demotion); only the account probe path opts into these shields.
/// Today:
/// - `*.taobao.com` / `*.tmall.com` / `*.tmall.hk` → `taobao-block-everyhelp`
///   (everyhelp getWidgetVersion answers 200 while Set-Cookie deletes
///   `unb`/`sn`/`uss` — session revocation with no error surface).
/// - `*.xiaohongshu.com` / `*.xhslink.com` → `xhs-sign` (logout fuse:
///   unsigned-API 406s otherwise trip the page's own `/sso/logout`).
///
/// Pinduoduo deliberately omitted — never wrap `window.fetch` (fbeZ
/// fingerprint). Doudian has no known cookie-deleting widget recipe today;
/// those platforms keep alive via `account/verify` write-back alone.
pub fn probe_shield_recipes(url: &str) -> Vec<&'static str> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    let mut out = Vec::new();
    if host_suffix_match(&host, &["taobao.com", "tmall.com", "tmall.hk"]) {
        out.push(TAOBAO_BLOCK_EVERYHELP_NAME);
    }
    if host_suffix_match(&host, &["xiaohongshu.com", "xhslink.com"]) {
        out.push(XHS_SIGN_NAME);
    }
    out
}

fn host_suffix_match(host: &str, roots: &[&str]) -> bool {
    roots.iter().any(|root| host == *root || host.ends_with(&format!(".{root}")))
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
        assert!(resolve("xhs-qrcode").is_some());
        assert!(resolve("x-p256").is_some());
        assert!(resolve("x-bearer").is_some());
        assert!(resolve("x-auth-ctx").is_some());
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
            "__xsStats",
            "_markNative",
            "_webmsxyw",
            "resolveSigner",
            "window.proxy",
            "waitSigner",
            "isXhsUrl",
            "sso\\/logout",
            "XMLHttpRequest.prototype.send",
            "window.fetch",
            "bump('swallowed')",
            "stats.passthrough",
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

    #[test]
    fn xhs_qrcode_script_carries_the_contract() {
        for needle in ["var qrcode", "createSvgTag", "globalThis.qrcode"] {
            assert!(XHS_QRCODE.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn x_p256_script_carries_the_contract() {
        for needle in ["globalThis.P256", "genKeyPair", "Jacobian", "0xffffffff00000001"] {
            assert!(X_P256.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn x_bearer_script_carries_the_contract() {
        for needle in ["globalThis.abxProbeBearer", "abx_bearer_at", "script[src*='main']", "Bearer "] {
            assert!(X_BEARER.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn x_auth_ctx_script_carries_the_contract() {
        for needle in [
            "globalThis.abxEnsureAuthCtx",
            "globalThis.abxB64keep",
            "globalThis.abxDerEncode",
            "globalThis.abxAuthHeaders",
            "globalThis.abxTxidGen",
            "abx_txid_ctx",
            "CreateWebSessionBind",
            "session_binding/challenge",
        ] {
            assert!(X_AUTH_CTX.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn probe_shields_taobao_and_tmall() {
        for url in [
            "https://www.taobao.com/",
            "https://login.taobao.com/member/login.jhtml",
            "https://myseller.taobao.com/home.htm/QnworkbenchHome/",
            "https://item.upload.taobao.com/",
            "https://www.tmall.com/",
            "https://pass.tmall.hk/",
        ] {
            assert_eq!(
                probe_shield_recipes(url),
                vec![TAOBAO_BLOCK_EVERYHELP_NAME],
                "TB/tmall probe must shield everyhelp: {url}"
            );
        }
    }

    #[test]
    fn probe_shields_xhs() {
        for url in [
            "https://www.xiaohongshu.com/explore",
            "https://creator.xiaohongshu.com/publish/publish",
            "https://ark.xiaohongshu.com/app-system/home",
            "https://customer.xiaohongshu.com/login",
            "https://xhslink.com/aBc",
        ] {
            assert_eq!(
                probe_shield_recipes(url),
                vec![XHS_SIGN_NAME],
                "XHS probe must mount logout fuse: {url}"
            );
        }
    }

    #[test]
    fn probe_shields_skip_pdd_doudian_and_blank() {
        // No fetch wrap on PDD; doudian has no cookie-kill recipe today.
        for url in [
            "https://mms.pinduoduo.com/home/",
            "https://fxg.jinritemai.com/ffa/mshop/homepage/index",
            "https://example.com/",
            "about:blank",
        ] {
            assert!(
                probe_shield_recipes(url).is_empty(),
                "no probe shield expected: {url}"
            );
        }
        // Auto-mount stays off even where probe shields apply.
        assert!(builtin_names("https://www.taobao.com/").is_empty());
        assert!(builtin_names("https://www.xiaohongshu.com/").is_empty());
    }
}

