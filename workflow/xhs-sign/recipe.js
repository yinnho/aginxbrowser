(() => {
  if (window.__xs_installed) return 'already';
  window.__xs_installed = true;
  // Qiankun/single-spa (ark itemark) may put the AS signer on window.proxy
  // only — window._webmsxyw stays undefined. Resolve lazily at sign time and
  // mirror onto window so page code that looks at window._webmsxyw also wins.
  const resolveSigner = () => {
    try {
      if (typeof window._webmsxyw === 'function') return window._webmsxyw;
      if (window.proxy && typeof window.proxy._webmsxyw === 'function') return window.proxy._webmsxyw;
    } catch (e) {}
    return null;
  };
  const mirrorSigner = () => {
    try {
      if (typeof window._webmsxyw === 'function') return window._webmsxyw;
      const s = resolveSigner();
      if (!s) return null;
      window._webmsxyw = s;
      window.__xs_mirrored = (window.__xs_mirrored || 0) + 1;
      return window._webmsxyw;
    } catch (e) { return null; }
  };
  mirrorSigner();
  const mirrorTimer = setInterval(mirrorSigner, 200);
  setTimeout(() => { try { clearInterval(mirrorTimer); } catch (e) {} }, 60000);
  // Only production xhs hosts — never sit/SSO ticket hosts (signing those
  // with the itemark AS signer can bounce the jar into customer.sit).
  const isXhsUrl = (u) => {
    const s = String(u || '');
    if (/customer\.sit\.|\/\/sit\.|\.sit\.xiaohongshu/i.test(s)) return false;
    if (s.indexOf('xiaohongshu.com') !== -1) return true;
    try {
      if (s.charAt(0) === '/' && /(?:^|\.)xiaohongshu\.com$/i.test(location.hostname)
          && !/\.sit\.xiaohongshu/i.test(location.hostname)) return true;
    } catch (e) {}
    return false;
  };
  // Auto-attach signatures for API calls; resign whenever page already set x-s.
  const isApiUrl = (u) => /\/api\//.test(String(u || ''));
  const sign = (u, b) => {
    try {
      const fn = mirrorSigner() || resolveSigner();
      if (typeof fn !== 'function') return null;
      const s = String(u);
      if (!isXhsUrl(s)) return null;
      const path = s.replace(/^(https?:)?\/\/[^\/]+/, '');
      let data = b;
      if (typeof b === 'string') { try { data = JSON.parse(b); } catch (e) {} }
      if (b == null || b instanceof FormData || b instanceof ArrayBuffer || b instanceof Blob) data = undefined;
      const sg = fn(path, data);
      return sg && sg['X-s'] ? sg : null;
    } catch (e) { return null; }
  };
  // Ark itemark mounts the signer a beat after first publish_render — wait briefly.
  const SIGNER_WAIT_MS = 2500;
  const waitSigner = (ms) => new Promise((resolve) => {
    const fn0 = mirrorSigner() || resolveSigner();
    if (fn0) { resolve(fn0); return; }
    const start = Date.now();
    const t = setInterval(() => {
      const fn = mirrorSigner() || resolveSigner();
      if (fn || Date.now() - start >= ms) {
        clearInterval(t);
        resolve(fn || null);
      }
    }, 50);
  });
  const FUSE = /\/sso\/logout|\/api\/galaxy\/user\/logout/;
  const O = XMLHttpRequest.prototype.open, S = XMLHttpRequest.prototype.setRequestHeader, D = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function(m, u) {
    this.__xs_u = String(u); this.__xs_m = String(m || 'GET').toUpperCase();
    return O.apply(this, arguments);
  };
  XMLHttpRequest.prototype.setRequestHeader = function(k, v) {
    const lk = String(k).toLowerCase();
    if (lk === 'x-s' || lk === 'x-t') {
      // #84: swallow only when we can re-sign at send(). Prefer proxy/_webmsxyw
      // (ark itemark) so we do not passthrough-then-406 when signer is sandboxed.
      if (typeof (mirrorSigner() || resolveSigner()) === 'function') {
        window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
        return;
      }
      // Signer may still be mounting — remember and swallow; send() waits then re-signs.
      if (isXhsUrl(this.__xs_u || '') && isApiUrl(this.__xs_u || '')) {
        this.__xs_pending_sign = true;
        window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
        return;
      }
      window.__xs_passthrough = (window.__xs_passthrough || 0) + 1;
    }
    return S.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function(b) {
    const self = this;
    const args = arguments;
    const finish = () => {
      try {
        if (FUSE.test(self.__xs_u || '')) {
          window.__xs_fuse = (window.__xs_fuse || 0) + 1;
          try {
            Object.defineProperty(self, 'readyState', { value: 4 });
            Object.defineProperty(self, 'status', { value: 200 });
            Object.defineProperty(self, 'responseText', { value: '{"code":0,"success":true}' });
            setTimeout(() => { try {
              self.dispatchEvent(new Event('readystatechange'));
              self.dispatchEvent(new Event('load'));
              self.dispatchEvent(new Event('loadend'));
            } catch (e) {} }, 0);
            return;
          } catch (e) { /* polyfill shape differs: fall through to the wire */ }
        }
        if (self.__xs_u && !(b instanceof FormData) && isXhsUrl(self.__xs_u)) {
          const sg = sign(self.__xs_u, b);
          if (sg) {
            S.call(self, 'X-s', String(sg['X-s']));
            S.call(self, 'X-t', String(sg['X-t']));
            window.__xs_signed = (window.__xs_signed || 0) + 1;
          }
        }
      } catch (e) {}
      return D.apply(self, args);
    };
    try {
      const u = this.__xs_u || '';
      if (isXhsUrl(u) && isApiUrl(u) && !resolveSigner() && !mirrorSigner()) {
        waitSigner(SIGNER_WAIT_MS).then(() => finish());
        return;
      }
    } catch (e) {}
    return finish();
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
    const body = (o.body != null && typeof o.body !== 'string') ? undefined : o.body;
    const headerHasXs = () => {
      const h = o.headers;
      if (!h || Array.isArray(h)) return false;
      if (h instanceof Headers) return h.has('x-s') || h.has('X-s');
      return ('x-s' in h || 'X-s' in h);
    };
    const apply = (sg) => {
      const h = o.headers;
      const has = headerHasXs();
      if (!sg) {
        if (has) window.__xs_passthrough = (window.__xs_passthrough || 0) + 1;
        return F.call(this, u, o);
      }
      let nh;
      if (!h || Array.isArray(h)) {
        nh = { 'x-s': String(sg['X-s']), 'x-t': String(sg['X-t']) };
      } else if (!(h instanceof Headers)) {
        nh = Object.assign({}, h);
        delete nh['x-s']; delete nh['X-s']; delete nh['x-t']; delete nh['X-t'];
        nh['x-s'] = String(sg['X-s']); nh['x-t'] = String(sg['X-t']);
      } else {
        nh = new Headers(h);
        nh.delete('x-s'); nh.delete('X-s'); nh.delete('x-t'); nh.delete('X-t');
        nh.set('x-s', String(sg['X-s'])); nh.set('x-t', String(sg['X-t']));
      }
      if (has) window.__xs_swallowed = (window.__xs_swallowed || 0) + 1;
      window.__xs_signed = (window.__xs_signed || 0) + 1;
      return F.call(this, u, Object.assign({}, o, { headers: nh }));
    };
    if (!isXhsUrl(us)) return F.call(this, u, o);
    const has = headerHasXs();
    // Resign when page set x-s; also auto-sign /api/* (ark publish_render omits x-s
    // when window._webmsxyw is missing). Skip non-API without x-s.
    if (!has && !isApiUrl(us)) return F.call(this, u, o);
    let sg = sign(us, body);
    if (sg) return apply(sg);
    if (!has && !isApiUrl(us)) return apply(null);
    return waitSigner(SIGNER_WAIT_MS).then(() => apply(sign(us, body)));
  };
  return 'patched';
})()
