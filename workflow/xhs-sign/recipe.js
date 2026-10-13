(() => {
  // v5: fingerprint-clean — wrappers take native shape ([native code] toString
  // via _markNative, no own .prototype, non-constructible), counters live in a
  // closure behind one non-enumerable __xsStats(), XHR instance props hidden.
  // 300015 "risk plugin" probes enumerate window keys and diff
  // Function.prototype.toString — v4 leaked the whole recipe source there.
  const XS_VER = 5;
  let prev = null;
  try { if (typeof window.__xsStats === 'function') prev = window.__xsStats(); } catch (e) {}
  if (prev && prev.ver >= XS_VER) return 'already';
  const stats = {
    ver: XS_VER, installed: 1, mirrored: 0, arkified: 0, swallowed: 0,
    passthrough: 0, passthrough_coherent: 0, common_stripped: 0, signed: 0,
    pr_held: 0, pr_blocked: 0, fuse: 0,
  };
  const bump = (k) => { stats[k] = (stats[k] || 0) + 1; };
  try {
    Object.defineProperty(window, '__xsStats', {
      value: function () { return JSON.parse(JSON.stringify(stats)); },
      writable: true, configurable: true, enumerable: false,
    });
  } catch (e) { window.__xsStats = function () { return JSON.parse(JSON.stringify(stats)); }; }
  // Hidden own props on wrapped XHR instances (plain assignment would show in
  // Object.keys(xhr) — Chrome instances have no own enumerable props).
  const def = (o, k, v) => {
    try { Object.defineProperty(o, k, { value: v, writable: true, configurable: true }); }
    catch (e) { try { o[k] = v; } catch (e2) {} }
  };
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
      bump('mirrored');
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
  // Itemark create SPA races publish_render before AS signer is healthy; gate it.
  const isPublishRender = (u) => /\/api\/edith\/product\/publish_render\b/i.test(String(u || ''));
  // Keep merchant product APIs on ark (SPA sometimes hops to www after env reject).
  const arkify = (u) => {
    const s = String(u || '');
    if (/^https?:\/\/(?:www\.)?xiaohongshu\.com\/api\/edith\/product\//i.test(s)) {
      return 'https://ark.xiaohongshu.com' + s.replace(/^https?:\/\/[^/]+/i, '');
    }
    try {
      if (s.charAt(0) === '/' && /\/api\/edith\/product\//i.test(s)
          && /(?:^|\.)xiaohongshu\.com$/i.test(location.hostname)
          && !/^ark\./i.test(location.hostname)
          && !/\.sit\.xiaohongshu/i.test(location.hostname)) {
        return 'https://ark.xiaohongshu.com' + s;
      }
    } catch (e) {}
    return s;
  };
  const sign = (u, b) => {
    try {
      const fn = mirrorSigner() || resolveSigner();
      if (typeof fn !== 'function') return null;
      const s = String(u);
      if (!isXhsUrl(s) && !isPublishRender(s)) return null;
      const path = s.replace(/^(https?:)?\/\/[^/]+/, '');
      let data = b;
      if (typeof b === 'string') { try { data = JSON.parse(b); } catch (e) {} }
      if (b == null || b instanceof FormData || b instanceof ArrayBuffer || b instanceof Blob) data = undefined;
      const sg = fn(path, data);
      return sg && sg['X-s'] ? sg : null;
    } catch (e) { return null; }
  };
  const commonFromSg = (sg) => {
    if (!sg) return null;
    const v = sg['X-s-common'] || sg['x-s-common'];
    return v != null && v !== '' ? String(v) : null;
  };
  // Ark itemark mounts the signer a beat after first publish_render — wait briefly.
  // Gated product APIs wait longer and require a short settle after first sighting.
  const SIGNER_WAIT_MS = 2500;
  const GATE_WAIT_MS = 12000;
  const SETTLE_MS = 2000;
  let signerSeenAt = 0;
  const noteSigner = () => {
    const fn = mirrorSigner() || resolveSigner();
    if (fn && !signerSeenAt) signerSeenAt = Date.now();
    return fn;
  };
  // Gated publish_render: signer + settle, and prefer at least one prior signed API
  // so Itemark/AS has exercised the native path (Oct-11 wait contract).
  const waitSigner = (ms, settle) => new Promise((resolve) => {
    const needSettle = !!settle;
    const ready = () => {
      const fn = noteSigner();
      if (!fn) return null;
      if (needSettle) {
        if (Date.now() - signerSeenAt < SETTLE_MS) return null;
        // If nothing else has been signed yet, keep holding until settle*1.5 or a sign lands.
        if (!(stats.signed > 0) && !(stats.passthrough > 0)
            && Date.now() - signerSeenAt < SETTLE_MS * 1.5) return null;
      }
      return fn;
    };
    const fn0 = ready();
    if (fn0) { resolve(fn0); return; }
    const start = Date.now();
    const t = setInterval(() => {
      const fn = ready();
      if (fn || Date.now() - start >= ms) {
        clearInterval(t);
        resolve(fn || noteSigner() || null);
      }
    }, 50);
  });
  const FUSE = /\/sso\/logout|\/api\/galaxy\/user\/logout/;
  const O = XMLHttpRequest.prototype.open, S = XMLHttpRequest.prototype.setRequestHeader, D = XMLHttpRequest.prototype.send;
  // Method shorthand (not function expressions): caller-bound `this`, no own
  // .prototype, non-constructible — the native method shape. _markNative then
  // makes Function.prototype.toString report [native code] for each wrapper.
  const XM = {
    open(m, u) {
      let uu = String(u);
      if (isPublishRender(uu)) {
        const a = arkify(uu);
        if (a !== uu) {
          uu = a;
          bump('arkified');
          arguments[1] = uu;
        }
        def(this, '__xs_pr', true);
      }
      def(this, '__xs_u', uu); def(this, '__xs_m', String(m || 'GET').toUpperCase());
      def(this, '__xs_page', {});
      return O.apply(this, arguments);
    },
    setRequestHeader(k, v) {
      const lk = String(k).toLowerCase();
      const xhsish = this.__xs_pr || isPublishRender(this.__xs_u || '')
        || isXhsUrl(this.__xs_u || '') || (this.__xs_u || '').charAt(0) === '/';
      if (lk === 'x-s' || lk === 'x-t' || lk === 'x-s-common') {
        def(this, '__xs_page', this.__xs_page || {});
        this.__xs_page[lk] = String(v);
      }
      // Defer x-s-common on xhs/API so resign can omit it (XHR cannot delete headers).
      if (lk === 'x-s-common' && xhsish) {
        def(this, '__xs_common_deferred', true);
        return;
      }
      if (lk === 'x-s' || lk === 'x-t') {
        // Swallow page x-s/x-t; decide passthrough vs resign at send() after gate.
        if (this.__xs_pr || isPublishRender(this.__xs_u || '')) {
          bump('swallowed');
          return;
        }
        // #84: swallow only when we can re-sign at send(). Prefer proxy/_webmsxyw
        // (ark itemark) so we do not passthrough-then-406 when signer is sandboxed.
        if (typeof (mirrorSigner() || resolveSigner()) === 'function') {
          bump('swallowed');
          return;
        }
        // Signer may still be mounting — swallow all xhs x-s/x-t (not only /api/)
        // so early passthrough cannot race ahead of the gate.
        if (xhsish) {
          bump('swallowed');
          return;
        }
        bump('passthrough');
      }
      return S.apply(this, arguments);
    },
    send(b) {
      const self = this;
      const args = arguments;
      const finish = (allowUnsigned) => {
        try {
          if (FUSE.test(self.__xs_u || '')) {
            bump('fuse');
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
          if (self.__xs_u && !(b instanceof FormData) && (isXhsUrl(self.__xs_u) || self.__xs_pr)) {
            const page = self.__xs_page || {};
            const hasPageXs = !!page['x-s'];
            const hasPageCommon = !!page['x-s-common'];
            const signerReady = typeof (mirrorSigner() || resolveSigner()) === 'function';
            // PRIMARY (Oct-11): signer ready + page already set x-s (+ ideally x-s-common) → passthrough
            if (signerReady && hasPageXs) {
              S.call(self, 'X-s', page['x-s']);
              if (page['x-t']) S.call(self, 'X-t', page['x-t']);
              if (hasPageCommon) S.call(self, 'X-s-common', page['x-s-common']);
              bump('passthrough');
              if (hasPageCommon) bump('passthrough_coherent');
              return D.apply(self, args);
            }
            // ELSE resign: set X-s/X-t; strip deferred x-s-common unless signer returns it
            const sg = sign(self.__xs_u, b);
            if (sg) {
              S.call(self, 'X-s', String(sg['X-s']));
              S.call(self, 'X-t', String(sg['X-t']));
              const cm = commonFromSg(sg);
              if (cm) {
                S.call(self, 'X-s-common', cm);
              } else if (self.__xs_common_deferred || hasPageCommon) {
                bump('common_stripped');
                // intentionally omit page x-s-common — never pair stale common with new X-s/X-t
              }
              bump('signed');
            } else if (self.__xs_pr && !allowUnsigned) {
              // Do not wire unsigned publish_render — synthetic soft fail (avoid 300015 poison).
              bump('pr_blocked');
              try {
                Object.defineProperty(self, 'readyState', { value: 4 });
                Object.defineProperty(self, 'status', { value: 200 });
                Object.defineProperty(self, 'responseText', {
                  value: JSON.stringify({ code: -1, success: false, msg: 'xs-gate:signer-not-ready' })
                });
                setTimeout(() => { try {
                  self.dispatchEvent(new Event('readystatechange'));
                  self.dispatchEvent(new Event('load'));
                  self.dispatchEvent(new Event('loadend'));
                } catch (e) {} }, 0);
                return;
              } catch (e) { return; }
            }
          }
        } catch (e) {}
        return D.apply(self, args);
      };
      try {
        const u = this.__xs_u || '';
        if (this.__xs_pr || isPublishRender(u)) {
          bump('pr_held');
          waitSigner(GATE_WAIT_MS, true).then(() => finish(false));
          return;
        }
        if (isXhsUrl(u) && isApiUrl(u) && !resolveSigner() && !mirrorSigner()) {
          waitSigner(SIGNER_WAIT_MS, false).then(() => finish(true));
          return;
        }
      } catch (e) {}
      return finish(true);
    },
  };
  XMLHttpRequest.prototype.open = XM.open;
  XMLHttpRequest.prototype.setRequestHeader = XM.setRequestHeader;
  XMLHttpRequest.prototype.send = XM.send;
  const F = window.fetch;
  const FM = {
    fetch(u, o) {
      o = o || {};
      let us = String(typeof u === 'object' && u ? u.url : u);
      if (isPublishRender(us)) {
        const a = arkify(us);
        if (a !== us) {
          bump('arkified');
          us = a;
          if (typeof u === 'string') u = a;
          else if (typeof Request !== 'undefined' && u instanceof Request) u = new Request(a, u);
          else u = a;
        }
      }
      if (FUSE.test(us)) {
        bump('fuse');
        return Promise.resolve(new Response(JSON.stringify({ code: 0, success: true }),
          { status: 200, headers: { 'content-type': 'application/json' } }));
      }
      const body = (o.body != null && typeof o.body !== 'string') ? undefined : o.body;
      const apply = (sg, gated) => {
        const h = o.headers;
        const hasXs = headerHas(h, ['x-s', 'X-s']);
        const hasCommon = headerHas(h, ['x-s-common', 'X-s-common']);
        const signerReady = typeof (mirrorSigner() || resolveSigner()) === 'function';
        // PRIMARY (Oct-11): signer ready + page already set x-s (+ ideally x-s-common) → passthrough
        if (signerReady && hasXs) {
          bump('passthrough');
          if (hasCommon) bump('passthrough_coherent');
          return F.call(this, u, o);
        }
        if (!sg) {
          if (gated) {
            bump('pr_blocked');
            return Promise.resolve(new Response(
              JSON.stringify({ code: -1, success: false, msg: 'xs-gate:signer-not-ready' }),
              { status: 200, headers: { 'content-type': 'application/json' } }
            ));
          }
          if (hasXs) bump('passthrough');
          return F.call(this, u, o);
        }
        // ELSE resign: strip x-s-common unless signer returns it atomically
        const cm = commonFromSg(sg);
        let nh;
        if (!h || Array.isArray(h)) {
          nh = { 'x-s': String(sg['X-s']), 'x-t': String(sg['X-t']) };
          if (cm) nh['x-s-common'] = cm;
          if (hasCommon && !cm) bump('common_stripped');
        } else if (!(h instanceof Headers)) {
          nh = Object.assign({}, h);
          delete nh['x-s']; delete nh['X-s']; delete nh['x-t']; delete nh['X-t'];
          delete nh['x-s-common']; delete nh['X-s-common'];
          // also strip any mixed-case variant
          for (const k of Object.keys(nh)) {
            const lk = String(k).toLowerCase();
            if (lk === 'x-s' || lk === 'x-t' || lk === 'x-s-common') delete nh[k];
          }
          nh['x-s'] = String(sg['X-s']); nh['x-t'] = String(sg['X-t']);
          if (cm) nh['x-s-common'] = cm;
          else if (hasCommon) bump('common_stripped');
        } else {
          nh = new Headers(h);
          nh.delete('x-s'); nh.delete('X-s'); nh.delete('x-t'); nh.delete('X-t');
          nh.delete('x-s-common'); nh.delete('X-s-common');
          nh.set('x-s', String(sg['X-s'])); nh.set('x-t', String(sg['X-t']));
          if (cm) nh.set('x-s-common', cm);
          else if (hasCommon) bump('common_stripped');
        }
        if (hasXs) bump('swallowed');
        bump('signed');
        return F.call(this, u, Object.assign({}, o, { headers: nh }));
      };
      if (!isXhsUrl(us) && !isPublishRender(us)) return F.call(this, u, o);
      const has = headerHas(o.headers, ['x-s', 'X-s']);
      const gated = isPublishRender(us);
      // Resign when page set x-s; also auto-sign /api/* (ark publish_render omits x-s
      // when window._webmsxyw is missing). Skip non-API without x-s (unless gated).
      if (!gated && !has && !isApiUrl(us)) return F.call(this, u, o);
      if (gated) {
        bump('pr_held');
        return waitSigner(GATE_WAIT_MS, true).then(() => apply(sign(us, body), true));
      }
      // If page already set x-s and signer ready — passthrough without waiting/resign
      if (has && typeof (mirrorSigner() || resolveSigner()) === 'function') {
        return apply(null, false); // apply() will take passthrough branch via hasXs
      }
      let sg = sign(us, body);
      if (sg) return apply(sg, false);
      if (!has && !isApiUrl(us)) return apply(null, false);
      return waitSigner(SIGNER_WAIT_MS, false).then(() => apply(sign(us, body), false));
    },
  };
  window.fetch = FM.fetch;
  try { Object.defineProperty(FM.fetch, 'length', { value: 0 }); } catch (e) {}
  const headerHas = (h, names) => {
    if (!h || Array.isArray(h)) return false;
    if (h instanceof Headers) {
      for (let i = 0; i < names.length; i++) if (h.has(names[i])) return true;
      return false;
    }
    for (let i = 0; i < names.length; i++) {
      if (names[i] in h) return true;
      const low = names[i].toLowerCase();
      for (const k of Object.keys(h)) if (String(k).toLowerCase() === low) return true;
    }
    return false;
  };
  // Engine's native-lie registry (bootstrap.js) — document-start runs after the
  // snapshot, and the global is hidden but callable there (fetch.js relies on
  // the same binding). Guarded so the recipe still installs (unmasked, but
  // functional) on realms without it.
  try {
    if (typeof _markNative === 'function') {
      _markNative(XM.open);
      _markNative(XM.setRequestHeader);
      _markNative(XM.send);
      _markNative(FM.fetch);
    }
  } catch (e) {}
  return 'patched-v5-fingerprint-clean';
})()
