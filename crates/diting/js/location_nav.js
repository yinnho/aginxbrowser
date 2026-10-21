// Location navigation (split from bootstrap.js): a navigation that differs
// from the current URL only by a non-null fragment stays in this document —
// it moves the history entry and fires hashchange, it does not refetch —
// and the part setters (hash/search/pathname/host/…) exist like Chrome's
// Location instead of silently ignoring writes. Everything is call-time.
(() => {
  const noFrag = (h) => String(h).split('#')[0];
  const scrollToFragment = (href) => {
    try {
      const i = href.indexOf('#');
      if (i < 0) return;
      let f = href.slice(i + 1);
      if (!f) return;
      try { f = decodeURIComponent(f); } catch {}
      const el = document.getElementById(f) || document.getElementsByName(f)[0];
      if (el) el.scrollIntoView();
    } catch {}
  };
  const go = (url, replace) => {
    const r = _resolveUrl(String(url));
    const cur = __currentUrl();
    if (r.indexOf('#') >= 0 && noFrag(r) === noFrag(cur)) {
      let n = r;
      try { n = new URL(r).href; } catch {}
      if (n !== cur) {
        if (replace) history.replaceState(history.state, '', n);
        else history.pushState(null, '', n);
      }
      scrollToFragment(n);
      return;
    }
    globalThis.__virtualUrl = r;
    _OPS.op_navigate(r, 'GET', '');
  };
  Object.defineProperty(globalThis, '__diting_locNav', { value: go, configurable: true, writable: true });
  const L = _locationObj;
  const def = (name, desc) => Object.defineProperty(L, name, { enumerable: true, configurable: true, ...desc });
  const hrefGet = Object.getOwnPropertyDescriptor(L, 'href').get;
  def('href', { get: hrefGet, set(v) { go(v, false); } });
  L.assign = function assign(url) { go(url, false); };
  L.replace = function replace(url) { go(url, true); };
  for (const name of ['protocol', 'host', 'hostname', 'port', 'pathname', 'search']) {
    const get = Object.getOwnPropertyDescriptor(L, name).get;
    def(name, { get, set(v) {
      let u;
      try { u = new URL(__currentUrl()); u[name] = String(v); } catch { return; }
      go(u.href, false);
    } });
  }
  const hashGet = Object.getOwnPropertyDescriptor(L, 'hash').get;
  def('hash', { get: hashGet, set(v) {
    v = String(v);
    if (v[0] === '#') v = v.slice(1);
    let n;
    try { n = new URL('#' + v, __currentUrl()).href; } catch { return; }
    go(n, false);
  } });
})();
