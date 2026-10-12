(() => {
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
      // #84: swallow only when we can re-sign at send(). On Ark the AS
      // signer never mounts (_webmsxyw undefined) — swallowing then leaves
      // the request bare → guaranteed 406 (xs_swallowed>0, xs_signed=0).
      if (typeof window._webmsxyw === 'function') {
        window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
        return;
      }
      window.__xs_passthrough = (window.__xs_passthrough || 0) + 1;
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
    // #84: swallow-then-resign only when _webmsxyw can re-sign. Otherwise
    // pass the page headers through (Ark: signer never mounts → bare 406).
    const body = (o.body != null && typeof o.body !== 'string') ? undefined : o.body;
    const sg = sign(us, body);
    if (!sg) {
      window.__xs_passthrough = (window.__xs_passthrough || 0) + 1;
      return F.call(this, u, o);
    }
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
})()
