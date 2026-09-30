//! Builtin document-start recipes and per-host auto-mounting (#84).
//!
//! session_preload (802c3d7) shipped the document-start hook but no
//! content: the xhs resign wrapper lived only in workflow/xhs-post's
//! flow.json, so a plain MCP caller could not have fixed the stuck
//! comments even on a release that carried the mechanism — the field
//! report that reopened #84. Builtin recipes close the gap from both
//! ends: the engine auto-mounts them on matching hosts (zero-config,
//! the reporter's option 2), and session_preload takes a `recipe` name
//! for explicit/off-domain use (option 1). Navigate responses list what
//! was applied (option 3).

/// Canonical recipe name for the xhs signing wrapper.
pub const XHS_SIGN_NAME: &str = "xhs-sign";

/// The xhs document-start signing wrapper (#84, batch 286).
///
/// Lineage: the eval-time mega patch from workflow/xhs-post/flow.json
/// (live-proven on the publish path), reshaped for the www read path.
/// The page VM's own x-s is computed inside this engine and comes out
/// wrong (comment/page 406), so where the mega patch signs only when
/// missing, this variant swallows the page's x-s/x-t and re-issues both
/// at send time — the semantics measured on the CDP bridge
/// (comment/page 461→200). Document-start is what makes it win at all:
/// the inline jsvmp captures window.fetch/XHR natives at parse time, so
/// the natives it captures are these wrappers. The same
/// `__xs_installed` flag means the publish flow's eval patch sees it
/// and bails instead of double-wrapping. The logout fuse stays
/// (2026-09-24: unsigned-API 406s misread as session death trigger the
/// page's own /sso/logout; synthetic 200s defuse it).
pub const XHS_SIGN: &str = r#"(() => {
  if (window.__xs_installed) return 'already';
  window.__xs_installed = true;
  const sign = (u, b) => {
    try {
      if (typeof window._webmsxyw !== 'function') return null;
      const s = String(u);
      if (s.indexOf('xiaohongshu.com') === -1) return null;
      const path = s.replace(/^(https?:)?\/\/[^\/]+/, '');
      let data = b;
      if (typeof b === 'string') { try { data = JSON.parse(b); } catch (e) {} }
      if (b == null || b instanceof FormData || b instanceof ArrayBuffer || b instanceof Blob) data = undefined;
      const sg = window._webmsxyw(path, data);
      return sg && sg['X-s'] ? sg : null;
    } catch (e) { return null; }
  };
  const FUSE = /\/sso\/logout|\/api\/galaxy\/user\/logout/;
  const O = XMLHttpRequest.prototype.open, S = XMLHttpRequest.prototype.setRequestHeader, D = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function(m, u) {
    this.__xs_u = String(u); this.__xs_m = String(m || 'GET').toUpperCase();
    return O.apply(this, arguments);
  };
  XMLHttpRequest.prototype.setRequestHeader = function(k, v) {
    const lk = String(k).toLowerCase();
    if (lk === 'x-s' || lk === 'x-t') {
      // #84: swallow the page's in-engine signature — send() re-issues both
      // fresh. Forwarding it would combine into a two-value header.
      window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
      return;
    }
    return S.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function(b) {
    try {
      if (FUSE.test(this.__xs_u || '')) {
        window.__xs_fuse = (window.__xs_fuse || 0) + 1;
        try {
          Object.defineProperty(this, 'readyState', { value: 4 });
          Object.defineProperty(this, 'status', { value: 200 });
          Object.defineProperty(this, 'responseText', { value: '{"code":0,"success":true}' });
          setTimeout(() => { try {
            this.dispatchEvent(new Event('readystatechange'));
            this.dispatchEvent(new Event('load'));
            this.dispatchEvent(new Event('loadend'));
          } catch (e) {} }, 0);
          return;
        } catch (e) { /* polyfill shape differs: fall through to the wire */ }
      }
      if (this.__xs_u && !(b instanceof FormData)) {
        const sg = sign(this.__xs_u, b);
        if (sg) {
          S.call(this, 'X-s', String(sg['X-s']));
          S.call(this, 'X-t', String(sg['X-t']));
          window.__xs_signed = (window.__xs_signed || 0) + 1;
        }
      }
    } catch (e) {}
    return D.apply(this, arguments);
  };
  const F = window.fetch;
  window.fetch = function(u, o) {
    o = o || {};
    const us = String(typeof u === 'object' && u ? u.url : u);
    if (FUSE.test(us)) {
      window.__xs_fuse = (window.__xs_fuse || 0) + 1;
      return Promise.resolve(new Response(JSON.stringify({ code: 0, success: true }),
        { status: 200, headers: { 'content-type': 'application/json' } }));
    }
    const h = o.headers;
    if (!h || Array.isArray(h)) return F.call(this, u, o);
    const plain = !(h instanceof Headers);
    const has = plain ? ('x-s' in h || 'X-s' in h) : (h.has('x-s') || h.has('X-s'));
    if (!has) return F.call(this, u, o);
    // #84: same swallow-then-resign on the fetch face — the jsvmp wrapper
    // posts half the boot burst through fetch with the page's own x-s.
    const body = (o.body != null && typeof o.body !== 'string') ? undefined : o.body;
    const sg = sign(us, body);
    if (!sg) return F.call(this, u, o);
    let nh;
    if (plain) {
      nh = Object.assign({}, h);
      delete nh['x-s']; delete nh['X-s'];
      nh['x-s'] = String(sg['X-s']); nh['x-t'] = String(sg['X-t']);
    } else {
      nh = new Headers(h);
      nh.delete('x-s');
      nh.set('x-s', String(sg['X-s'])); nh.set('x-t', String(sg['X-t']));
    }
    window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
    window.__xs_signed = (window.__xs_signed || 0) + 1;
    return F.call(this, u, Object.assign({}, o, { headers: nh }));
  };
  return 'patched';
})()"#;

/// Resolve a recipe name (aliases included) to its source. None for
/// unknown names — callers surface the error rather than silently
/// mounting nothing.
pub fn resolve(name: &str) -> Option<&'static str> {
    match name {
        XHS_SIGN_NAME | "xhs" => Some(XHS_SIGN),
        _ => None,
    }
}

/// Builtin recipe scripts that apply to this navigation URL, in mount
/// order (before any user-set group).
fn builtin_scripts(url: &str) -> Vec<&'static str> {
    if site_preloads_disabled() {
        return Vec::new();
    }
    if is_xhs_url(url) {
        vec![XHS_SIGN]
    } else {
        Vec::new()
    }
}

/// Report-only view of [`builtin_scripts`] — the names surfaces quote
/// (navigate responses, docs) without the source body.
pub fn builtin_names(url: &str) -> Vec<&'static str> {
    if builtin_scripts(url).is_empty() {
        Vec::new()
    } else {
        vec![XHS_SIGN_NAME]
    }
}

/// The document-start group for a navigation: builtin recipes first,
/// then the user group unchanged.
pub fn effective_scripts(url: &str, user: &[String]) -> Vec<String> {
    builtin_scripts(url)
        .into_iter()
        .map(str::to_string)
        .chain(user.iter().cloned())
        .collect()
}

/// xiaohongshu.com and its subdomains, plus xhslink.com (the short-link
/// host — a requested xhslink.com URL redirects to a www document, and
/// the wrapper no-ops until the landing page defines `_webmsxyw`, so
/// mounting on the hop is what carries the recipe through the chain).
fn is_xhs_url(url: &str) -> bool {
    let Some(host) = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)) else {
        return false;
    };
    host == "xiaohongshu.com"
        || host.ends_with(".xiaohongshu.com")
        || host == "xhslink.com"
        || host.ends_with(".xhslink.com")
}

fn site_preloads_disabled() -> bool {
    site_preloads_disabled_from(std::env::var("AGINXBROWSER_DISABLE_SITE_PRELOAD").ok().as_deref())
}

fn site_preloads_disabled_from(v: Option<&str>) -> bool {
    v == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_matches_xiaohongshu_hosts_only() {
        for url in [
            "https://www.xiaohongshu.com/explore?channel_id=homefeed_recommend",
            "https://xiaohongshu.com/",
            "http://edith.xiaohongshu.com/api/sns/web/v1/feed",
            "https://xhslink.com/aBc123",
        ] {
            assert_eq!(builtin_names(url), vec!["xhs-sign"], "should match: {url}");
        }
        // Look-alikes must not match: host suffix, not substring.
        for url in [
            "https://example.com/xiaohongshu.com/path",
            "https://xiaohongshu.com.evil.io/",
            "https://notxiaohongshu.com/",
            "https://xhslink.com.evil.io/a",
            "about:blank",
            "not a url",
        ] {
            assert!(builtin_names(url).is_empty(), "should not match: {url}");
        }
    }

    #[test]
    fn effective_puts_builtin_before_user_group() {
        // Creation navigation (session_thread's initial goto) arrives here
        // with an empty user group — builtin-only is the whole mount.
        assert_eq!(
            effective_scripts("https://www.xiaohongshu.com/explore", &[]),
            vec![XHS_SIGN.to_string()]
        );
        let user = vec!["window.__mine = 1;".to_string()];
        let eff = effective_scripts("https://www.xiaohongshu.com/explore", &user);
        assert_eq!(eff.len(), 2);
        assert!(eff[0].contains("__xs_installed"), "builtin rides first");
        assert_eq!(eff[1], "window.__mine = 1;");
        // Off-domain: user group only, untouched.
        let off = effective_scripts("https://example.com/", &user);
        assert_eq!(off, user);
    }

    #[test]
    fn resolve_known_names_and_rejects_unknown() {
        assert!(resolve("xhs-sign").is_some());
        assert!(resolve("xhs").is_some());
        assert!(resolve("nope").is_none());
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
        ] {
            assert!(XHS_SIGN.contains(needle), "script lost: {needle}");
        }
    }

    #[test]
    fn kill_switch_is_exact_value() {
        assert!(site_preloads_disabled_from(Some("1")));
        assert!(!site_preloads_disabled_from(Some("0")));
        assert!(!site_preloads_disabled_from(Some("true")));
        assert!(!site_preloads_disabled_from(None));
    }
}
