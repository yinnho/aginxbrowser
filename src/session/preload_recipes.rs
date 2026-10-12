//! Named document-start recipes (opt-in) — no per-host auto-mount.
//!
//! Platform policy (signing wrappers, logout fuses, site quirks) lives in
//! `workflow/<name>/`, not in the engine's default navigation path. The
//! `xhs-sign` recipe remains callable via `POST /session/:id/preload`
//! `{"recipe":"xhs-sign"}` (or alias `"xhs"`) when a caller or flow wants
//! the maintained document-start wrapper without pasting the source —
//! but matching `*.xiaohongshu.com` / `*.xhslink.com` no longer mounts it
//! automatically (demoted 2026-10-12: delivery unit = engine + flow;
//! earlier auto-mount was #84 / batch 286, reversed to match the
//! BUILTIN_FLOWS revoke of 2026-10-02).
//!
//! Lineage of the script body: the eval-time mega patch from
//! `workflow/xhs-post/flow.json`, reshaped for the www read path
//! (swallow page x-s/x-t and re-sign — page VM signatures computed
//! in-engine come out wrong; CDP bridge measured comment/page 461→200).
//! Document-start is what makes it win before inline jsvmp captures
//! fetch/XHR natives. `__xs_installed` keeps the publish flow's eval
//! patch from double-wrapping. Logout fuse stays (unsigned-API 406s
//! misread as session death trigger the page's own /sso/logout).

/// Canonical recipe name for the xhs signing wrapper.
pub const XHS_SIGN_NAME: &str = "xhs-sign";

/// The xhs document-start signing wrapper (#84, batch 286).
///
/// Opt-in only — see module docs. Observability on `window`:
/// `__xs_installed` / `__xs_swallowed` / `__xs_signed` / `__xs_fuse`.
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
}
