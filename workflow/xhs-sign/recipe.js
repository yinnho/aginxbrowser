(() => {
  // v11: v10 + WASM probe/cache/blacklist; slower poll (hang guard). Live verify showed mnsv2/xy18
  // mounts but page/probe still hit www _webmsxyw → XYW_~668 (gate soft-
  // blocked). library-launcher fy() is: mnsv2(a,md5(a),md5(path)) → x3,
  // pack {x0..x5} → custom-b64 → XYS_. Embed that for ark resign + keep
  // page x-s-common (fU). Native mnsv2 alias still missing.
  const XS_VER = 11;
  let prev = null;
  try { if (typeof window.__xsStats === 'function') prev = window.__xsStats(); } catch (e) {}
  if (prev && prev.ver >= XS_VER) return 'already';
  const stats = {
    ver: XS_VER, installed: 1, mirrored: 0, arkified: 0, swallowed: 0,
    passthrough: 0, passthrough_coherent: 0, common_stripped: 0, signed: 0,
    pr_held: 0, pr_blocked: 0, fuse: 0, replayed: 0, dropped: 0,
    mns_mirrored: 0, mns_bridged: 0, xyw_blocked: 0, ark_xys: 0,
  };
  const bump = (k) => { stats[k] = (stats[k] || 0) + 1; };
  // Last publish_render decision + wire outcome (closure-only, surfaced via
  // __xsStats()). mode: passthrough|resign|blocked|unsigned-wire.
  stats.pr_last = null;
  const notePr = (mode, extra) => {
    stats.pr_last = Object.assign({ ts: Date.now(), mode: mode }, extra || {});
  };
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
  // Qiankun/single-spa (ark itemark) may put AS signer / mnsv2 on window.proxy
  // only. Resolve lazily and mirror onto window so page + recipe both win.
  const resolveSigner = () => {
    try {
      if (typeof window._webmsxyw === 'function') return window._webmsxyw;
      if (window.proxy && typeof window.proxy._webmsxyw === 'function') return window.proxy._webmsxyw;
    } catch (e) {}
    return null;
  };
  const resolveMnsv2 = () => {
    try {
      if (typeof window.mnsv2 === 'function') return window.mnsv2;
      if (window.proxy && typeof window.proxy.mnsv2 === 'function') return window.proxy.mnsv2;
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
  const mirrorMnsv2 = () => {
    try {
      if (typeof window.mnsv2 === 'function') return window.mnsv2;
      const s = resolveMnsv2();
      if (!s) return null;
      window.mnsv2 = s;
      try { if (window.proxy && typeof window.proxy.mnsv2 !== 'function') window.proxy.mnsv2 = s; } catch (e2) {}
      bump('mns_mirrored');
      return window.mnsv2;
    } catch (e) { return null; }
  };

  // TEMPORARY: synthesize window.mnsv2 from signV2Init WASM / VM crypto when
  // the jsvmp skips the native alias (see commit receipt). Prefer xy_18*
  // export (bytecode binds that name to mnsv2); fall back to a 16-byte
  // hashed crypto stub if present.
  const bytesToHex = (bytes, n) => {
    const lim = Math.min(n || 16, bytes.length || 0);
    let s = '';
    for (let i = 0; i < lim; i++) {
      const b = bytes[i] & 0xff;
      s += (b < 16 ? '0' : '') + b.toString(16);
    }
    return s;
  };
  let _wasmMnsCache = null; // {xy,malloc,memory} | false (probed fail)
  const findWasmMnsExport = () => {
    try {
      if (_wasmMnsCache === false) return null;
      if (_wasmMnsCache) return _wasmMnsCache;
      if (typeof WebAssembly === 'undefined' || !WebAssembly.Instance) return null;
      const names = Object.getOwnPropertyNames(window);
      for (let i = 0; i < names.length; i++) {
        let inst;
        try { inst = window[names[i]]; } catch (e) { continue; }
        if (!inst || !(inst instanceof WebAssembly.Instance) || !inst.exports) continue;
        const ex = inst.exports;
        if (typeof ex.malloc !== 'function' || !ex.memory) continue;
        let xy = null;
        const keys = Object.keys(ex);
        for (let k = 0; k < keys.length; k++) {
          if (/^xy_18/.test(keys[k]) && typeof ex[keys[k]] === 'function') { xy = ex[keys[k]]; break; }
        }
        if (!xy) {
          for (let k = 0; k < keys.length; k++) {
            if (/^xy_/.test(keys[k]) && typeof ex[keys[k]] === 'function' && ex[keys[k]].length === 3) {
              xy = ex[keys[k]]; break;
            }
          }
        }
        if (xy) {
          // One-shot probe: if digest is zeros or throws, blacklist this Instance.
          try {
            const enc = typeof TextEncoder !== 'undefined' ? new TextEncoder() : null;
            const write = (s) => {
              const str = String(s == null ? '' : s);
              let u8;
              if (enc) u8 = enc.encode(str);
              else { u8 = new Uint8Array(str.length); for (let j = 0; j < str.length; j++) u8[j] = str.charCodeAt(j) & 0xff; }
              const ptr = ex.malloc(u8.length + 1);
              const he = new Uint8Array(ex.memory.buffer, ptr, u8.length + 1);
              he.set(u8); he[u8.length] = 0;
              return ptr;
            };
            const r = xy(write('xs'), write('bridge'), write('probe'));
            const bytes = new Uint8Array(ex.memory.buffer, r >>> 0, 16);
            let nz = false;
            for (let j = 0; j < 16; j++) if (bytes[j]) { nz = true; break; }
            if (!nz) { _wasmMnsCache = false; return null; }
          } catch (e) { _wasmMnsCache = false; return null; }
          _wasmMnsCache = { xy: xy, malloc: ex.malloc, memory: ex.memory };
          return _wasmMnsCache;
        }
      }
    } catch (e) {}
    return null;
  };
  const findCryptoStub = () => {
    // Probe hashed helpers: mns core returns a 16-byte array-like from 3 strings.
    // Guarded try/catch — other stubs throw or mutate; only accept clean 16-byte outs.
    try {
      const names = Object.getOwnPropertyNames(window);
      for (let i = 0; i < names.length; i++) {
        const k = names[i];
        if (!/^_[0-9a-f]{32}$/.test(k)) continue;
        const fn = window[k];
        if (typeof fn !== 'function') continue;
        try {
          const out = fn('xs', 'bridge', 'probe');
          if (out && typeof out === 'object' && out.length === 16) return fn;
        } catch (e) {}
      }
    } catch (e) {}
    return null;
  };
  const installMnsv2Bridge = () => {
    try {
      const cur = typeof window.mnsv2 === 'function' ? window.mnsv2 : null;
      if (cur && !cur.__xsBridge) return cur; // native won
      // Upgrade path: crypto-stub (or missing) → xy18 when WASM Instance appears.
      // Do NOT early-return on existing __xsBridge=crypto-stub.
      if (cur && cur.__xsBridge === 'xy18') return cur;
      let bridge = null;
      const wasm = findWasmMnsExport();
      if (wasm) {
        const enc = typeof TextEncoder !== 'undefined' ? new TextEncoder() : null;
        bridge = function mnsv2(a, b, d) {
          try {
            const write = (s) => {
              const str = String(s == null ? '' : s);
              let u8;
              if (enc) u8 = enc.encode(str);
              else {
                u8 = new Uint8Array(str.length);
                for (let i = 0; i < str.length; i++) u8[i] = str.charCodeAt(i) & 0xff;
              }
              const p = wasm.malloc(u8.length + 1);
              if (!p) throw new Error('malloc0');
              const he = new Uint8Array(wasm.memory.buffer, p, u8.length + 1);
              he.set(u8); he[u8.length] = 0;
              return p;
            };
            const r = wasm.xy(write(a), write(b), write(d));
            const bytes = new Uint8Array(wasm.memory.buffer, (r >>> 0), 16);
            const hex = bytesToHex(bytes, 16);
            if (/^0+$/.test(hex)) throw new Error('zero-digest');
            return hex;
          } catch (e) {
            _wasmMnsCache = false;
            throw e;
          }
        };
        bridge.__xsBridge = 'xy18';
      }
      if (!bridge && !(cur && cur.__xsBridge)) {
        const stub = findCryptoStub();
        if (stub) {
          bridge = function mnsv2(a, b, d) {
            const out = stub(a, b, d);
            return bytesToHex(out, 16);
          };
          bridge.__xsBridge = 'crypto-stub';
        }
      }
      if (!bridge) return cur || null;
      window.mnsv2 = bridge;
      try { if (window.proxy) window.proxy.mnsv2 = bridge; } catch (e2) {}
      if (!cur || cur.__xsBridge !== bridge.__xsBridge) bump('mns_bridged');
      return window.mnsv2;
    } catch (e) { return null; }
  };

  const isArkHost = () => {
    try { return /^ark\./i.test(location.hostname); } catch (e) { return false; }
  };
  // www-family XYW_ has no x-s-common; ark Chrome family is XYS_ + common.
  const sgFamily = (sg) => {
    try {
      const xs = String((sg && (sg['X-s'] || sg['x-s'])) || '');
      const cm = sg && (sg['X-s-common'] || sg['x-s-common']);
      return { prefix: xs.slice(0, 4), xsLen: xs.length, hasCommon: !!(cm != null && cm !== '') };
    } catch (e) { return { prefix: '', xsLen: 0, hasCommon: false }; }
  };
  const isXywPoison = (sg) => {
    const f = sgFamily(sg);
    return f.prefix === 'XYW_' || (f.xsLen > 0 && !f.hasCommon && f.xsLen > 500);
  };
  mirrorSigner();
  mirrorMnsv2();
  installMnsv2Bridge();
  const mirrorTimer = setInterval(() => {
    mirrorSigner();
    mirrorMnsv2();
    try {
      if (!(window.mnsv2 && window.mnsv2.__xsBridge === 'xy18')) installMnsv2Bridge();
    } catch (e) {}
  }, 1000);
  setTimeout(() => { try { clearInterval(mirrorTimer); } catch (e) {} }, 120000);
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
  // Relative URLs are same-origin; absolute ones compare host (incl. port).
  // Auto-signing a CROSS-origin URL the page never signed adds x-s → triggers
  // a preflight the server may not allow ("CORS preflight did not allow
  // request header 'x-s'", observed live on the www error page hop).
  const sameOrigin = (u) => {
    const m = String(u || '').match(/^(?:https?:)?\/\/([^/?#]+)/i);
    if (!m) return true;
    try { return m[1].toLowerCase() === location.host.toLowerCase(); } catch (e) { return true; }
  };
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

  // --- ark fy-replica (library-launcher fy without x6/x7 SSK; webSsk often empty) ---
  const XS_ALPH = 'ZmserbBoHQtNP+wOcza/LpngG8yJq42KWYj0DSfdikx3VT16IlUAFM97hECvuRX5';
  const XS_A = (() => { const a = []; for (let i = 0; i < XS_ALPH.length; i++) a[i] = XS_ALPH[i]; return a; })();
  const xsAf = (f) => {
    const e = encodeURIComponent(f);
    const c = [];
    for (let i = 0; i < e.length; i++) {
      const b = e.charAt(i);
      if (b === '%') { c.push(parseInt(e.charAt(i + 1) + e.charAt(i + 2), 16)); i += 2; }
      else c.push(b.charCodeAt(0));
    }
    return c;
  };
  const xsN3 = (f) => XS_A[f >> 18 & 63] + XS_A[f >> 12 & 63] + XS_A[f >> 6 & 63] + XS_A[63 & f];
  const xsEncChunk = (f, e, c) => {
    const out = [];
    for (let b = e; b < c; b += 3) out.push(xsN3((f[b] << 16 & 0xff0000) + (f[b + 1] << 8 & 65280) + (255 & f[b + 2])));
    return out.join('');
  };
  const xsTF = (f) => {
    const c = f.length, b = c % 3, d = [], x = 16383;
    let t = 0, n2 = c - b;
    for (; t < n2; t += x) d.push(xsEncChunk(f, t, t + x > n2 ? n2 : t + x));
    if (b === 1) { const e = f[c - 1]; d.push(XS_A[e >> 2] + XS_A[e << 4 & 63] + '=='); }
    else if (b === 2) { const e = (f[c - 2] << 8) + f[c - 1]; d.push(XS_A[e >> 10] + XS_A[e >> 4 & 63] + XS_A[e << 2 & 63] + '='); }
    return d.join('');
  };
  // paul johnston md5 (utf8) — matches fo.BD
  const xsMd5 = (s) => {
    const add32 = (a, b) => (a + b) & 0xFFFFFFFF;
    const cmn = (q, a, b, x, s, t) => {
      a = add32(add32(a, q), add32(x, t));
      return add32((a << s) | (a >>> (32 - s)), b);
    };
    const ff = (a, b, c, d, x, s, t) => cmn((b & c) | ((~b) & d), a, b, x, s, t);
    const gg = (a, b, c, d, x, s, t) => cmn((b & d) | (c & (~d)), a, b, x, s, t);
    const hh = (a, b, c, d, x, s, t) => cmn(b ^ c ^ d, a, b, x, s, t);
    const ii = (a, b, c, d, x, s, t) => cmn(c ^ (b | (~d)), a, b, x, s, t);
    const md5cycle = (x, k) => {
      let a = x[0], b = x[1], c = x[2], d = x[3];
      a = ff(a, b, c, d, k[0], 7, -680876936);
      d = ff(d, a, b, c, k[1], 12, -389564586);
      c = ff(c, d, a, b, k[2], 17, 606105819);
      b = ff(b, c, d, a, k[3], 22, -1044525330);
      a = ff(a, b, c, d, k[4], 7, -176418897);
      d = ff(d, a, b, c, k[5], 12, 1200080426);
      c = ff(c, d, a, b, k[6], 17, -1473231341);
      b = ff(b, c, d, a, k[7], 22, -45705983);
      a = ff(a, b, c, d, k[8], 7, 1770035416);
      d = ff(d, a, b, c, k[9], 12, -1958414417);
      c = ff(c, d, a, b, k[10], 17, -42063);
      b = ff(b, c, d, a, k[11], 22, -1990404162);
      a = ff(a, b, c, d, k[12], 7, 1804603682);
      d = ff(d, a, b, c, k[13], 12, -40341101);
      c = ff(c, d, a, b, k[14], 17, -1502002290);
      b = ff(b, c, d, a, k[15], 22, 1236535329);
      a = gg(a, b, c, d, k[1], 5, -165796510);
      d = gg(d, a, b, c, k[6], 9, -1069501632);
      c = gg(c, d, a, b, k[11], 14, 643717713);
      b = gg(b, c, d, a, k[0], 20, -373897302);
      a = gg(a, b, c, d, k[5], 5, -701558691);
      d = gg(d, a, b, c, k[10], 9, 38016083);
      c = gg(c, d, a, b, k[15], 14, -660478335);
      b = gg(b, c, d, a, k[4], 20, -405537848);
      a = gg(a, b, c, d, k[9], 5, 568446438);
      d = gg(d, a, b, c, k[14], 9, -1019803690);
      c = gg(c, d, a, b, k[3], 14, -187363961);
      b = gg(b, c, d, a, k[8], 20, 1163531501);
      a = gg(a, b, c, d, k[13], 5, -1444681467);
      d = gg(d, a, b, c, k[2], 9, -51403784);
      c = gg(c, d, a, b, k[7], 14, 1735328473);
      b = gg(b, c, d, a, k[12], 20, -1926607734);
      a = hh(a, b, c, d, k[5], 4, -378558);
      d = hh(d, a, b, c, k[8], 11, -2022574463);
      c = hh(c, d, a, b, k[11], 16, 1839030562);
      b = hh(b, c, d, a, k[14], 23, -35309556);
      a = hh(a, b, c, d, k[1], 4, -1530992060);
      d = hh(d, a, b, c, k[4], 11, 1272893353);
      c = hh(c, d, a, b, k[7], 16, -155497632);
      b = hh(b, c, d, a, k[10], 23, -1094730640);
      a = hh(a, b, c, d, k[13], 4, 681279174);
      d = hh(d, a, b, c, k[0], 11, -358537222);
      c = hh(c, d, a, b, k[3], 16, -722521979);
      b = hh(b, c, d, a, k[6], 23, 76029189);
      a = hh(a, b, c, d, k[9], 4, -640364487);
      d = hh(d, a, b, c, k[12], 11, -421815835);
      c = hh(c, d, a, b, k[15], 16, 530742520);
      b = hh(b, c, d, a, k[2], 23, -995338651);
      a = ii(a, b, c, d, k[0], 6, -198630844);
      d = ii(d, a, b, c, k[7], 10, 1126891415);
      c = ii(c, d, a, b, k[14], 15, -1416354905);
      b = ii(b, c, d, a, k[5], 21, -57434055);
      a = ii(a, b, c, d, k[12], 6, 1700485571);
      d = ii(d, a, b, c, k[3], 10, -1894986606);
      c = ii(c, d, a, b, k[10], 15, -1051523);
      b = ii(b, c, d, a, k[1], 21, -2054922799);
      a = ii(a, b, c, d, k[8], 6, 1873313359);
      d = ii(d, a, b, c, k[15], 10, -30611744);
      c = ii(c, d, a, b, k[6], 15, -1560198380);
      b = ii(b, c, d, a, k[13], 21, 1309151649);
      a = ii(a, b, c, d, k[4], 6, -145523070);
      d = ii(d, a, b, c, k[11], 10, -1120210379);
      c = ii(c, d, a, b, k[2], 15, 718787259);
      b = ii(b, c, d, a, k[9], 21, -343485551);
      x[0] = add32(a, x[0]); x[1] = add32(b, x[1]); x[2] = add32(c, x[2]); x[3] = add32(d, x[3]);
    };
    const md5blk = (s) => {
      const md5blks = [];
      for (let i = 0; i < 64; i += 4) {
        md5blks[i >> 2] = s.charCodeAt(i) + (s.charCodeAt(i + 1) << 8) + (s.charCodeAt(i + 2) << 16) + (s.charCodeAt(i + 3) << 24);
      }
      return md5blks;
    };
    const md51 = (s) => {
      const n = s.length;
      const state = [1732584193, -271733879, -1732584194, 271733878];
      let i;
      for (i = 64; i <= n; i += 64) md5cycle(state, md5blk(s.substring(i - 64, i)));
      s = s.substring(i - 64);
      const tail = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
      for (i = 0; i < s.length; i++) tail[i >> 2] |= s.charCodeAt(i) << ((i % 4) << 3);
      tail[i >> 2] |= 0x80 << ((i % 4) << 3);
      if (i > 55) { md5cycle(state, tail); for (i = 0; i < 16; i++) tail[i] = 0; }
      tail[14] = n * 8;
      md5cycle(state, tail);
      return state;
    };
    const hex_chr = '0123456789abcdef'.split('');
    const rhex = (n) => {
      let s = '', j = 0;
      for (; j < 4; j++) s += hex_chr[(n >> (j * 8 + 4)) & 0x0F] + hex_chr[(n >> (j * 8)) & 0x0F];
      return s;
    };
    s = unescape(encodeURIComponent(s));
    const x = md51(s);
    return rhex(x[0]) + rhex(x[1]) + rhex(x[2]) + rhex(x[3]);
  };
  const signArkXyS = (path, data) => {
    try {
      const mns = resolveMnsv2() || mirrorMnsv2() || installMnsv2Bridge();
      if (typeof mns !== 'function') return null;
      let a = String(path || '');
      if (data != null && typeof data === 'object') a += JSON.stringify(data);
      else if (typeof data === 'string') a += data;
      const b = xsMd5([a].join(''));
      const d = xsMd5(String(path || ''));
      // Prefer xy18; crypto-stub often returns zeros and 406s.
      if (mns.__xsBridge === 'crypto-stub') {
        const up = installMnsv2Bridge();
        if (!up || up.__xsBridge !== 'xy18') return null;
      }
      const x3 = (resolveMnsv2() || mns)(a, b, d);
      if (x3 == null || x3 === '') return null;
      const x3s = String(x3);
      if (/^0+$/.test(x3s)) return null;
      let plat = 'PC';
      try { plat = window.xsecplatform || (window.proxy && window.proxy.xsecplatform) || 'PC'; } catch (e) {}
      const n = {
        x0: '4.4.4',
        x1: 'itemark',
        x2: plat,
        x3: x3,
        x4: data != null ? (typeof data) : '',
        x5: b,
      };
      const xs = 'XYS_' + xsTF(xsAf(JSON.stringify(n)));
      if (xs.length < 20 || xs.slice(0, 4) !== 'XYS_') return null;
      bump('ark_xys');
      return { 'X-s': xs, 'X-t': String(Date.now()) };
    } catch (e) { return null; }
  };

  const sign = (u, b) => {
    try {
      const s = String(u);
      if (!isXhsUrl(s) && !isPublishRender(s)) return null;
      const path = s.replace(/^(https?:)?\/\/[^/]+/, '');
      let data = b;
      if (typeof b === 'string') { try { data = JSON.parse(b); } catch (e) {} }
      if (b == null || b instanceof FormData || b instanceof ArrayBuffer || b instanceof Blob) data = undefined;
      // Ark: mint XYS_ via mnsv2 fy-replica (never www XYW_).
      if (isArkHost()) {
        const xys = signArkXyS(path, data);
        if (xys && xys['X-s']) return xys;
      }
      const fn = mirrorSigner() || resolveSigner();
      if (typeof fn !== 'function') return null;
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
      // Ark create: hold until mnsv2 is up so page fy() can mint XYS_+common.
      // Without it, resign falls to www XYW_ and 300015s.
      if (needSettle && isArkHost() && !resolveMnsv2() && !mirrorMnsv2()) return null;
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
        // Timeout resolves null (fail closed): a signer that never satisfied
        // the settle contract must not sign publish_render late-and-raw.
        resolve(fn || null);
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
      let pr = false;
      if (isPublishRender(uu)) {
        const a = arkify(uu);
        if (a !== uu) {
          uu = a;
          bump('arkified');
          arguments[1] = uu;
        }
        pr = true;
      }
      // Reused XHR objects start each open() fresh (a publish_render open
      // must not stick PR treatment onto the next unrelated request).
      def(this, '__xs_pr', pr);
      def(this, '__xs_common_deferred', false);
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
      // Snapshot at send() time: a gate-held request must decide on the state
      // the page presented when send() was called. If the page reuses the XHR
      // (abort+open again) while the gate holds, live flags now describe the
      // NEW request — acting on them sends stale PR bodies to wrong URLs.
      const snap = {
        u: this.__xs_u || '',
        pr: !!this.__xs_pr,
        page: Object.assign({}, this.__xs_page || {}),
        deferred: !!this.__xs_common_deferred,
      };
      const finish = (allowUnsigned) => {
        try {
          // Reuse between send() and gate release: both wiring the stale body
          // and synthetically completing would corrupt the live second
          // request on the same object. Drop the ghost request entirely.
          if ((self.__xs_u || '') !== snap.u || !!self.__xs_pr !== snap.pr) {
            bump('dropped');
            return;
          }
          if (FUSE.test(snap.u)) {
            bump('fuse');
            try {
              Object.defineProperty(self, 'readyState', { value: 4, configurable: true });
              Object.defineProperty(self, 'status', { value: 200, configurable: true });
              Object.defineProperty(self, 'responseText', { value: '{"code":0,"success":true}', configurable: true });
              setTimeout(() => { try {
                self.dispatchEvent(new Event('readystatechange'));
                self.dispatchEvent(new Event('load'));
                self.dispatchEvent(new Event('loadend'));
              } catch (e) {} }, 0);
              return;
            } catch (e) { /* polyfill shape differs: fall through to the wire */ }
          }
          if (snap.pr) notePr('unsigned-wire', { body: b instanceof FormData ? 'FormData' : 'other' });
          if (snap.u && (isXhsUrl(snap.u) || snap.pr)) {
            const page = snap.page;
            const hasPageXs = !!page['x-s'];
            const hasPageCommon = !!page['x-s-common'];
            const signerReady = typeof (mirrorSigner() || resolveSigner()) === 'function';
            const formBody = b instanceof FormData;
            const cross = !sameOrigin(snap.u);
            const sg = formBody ? null : sign(snap.u, b);
            // PRIMARY (Oct-11): signer ready + page already set x-s (+ ideally x-s-common) → passthrough
            if (signerReady && hasPageXs && !formBody) {
              S.call(self, 'X-s', page['x-s']);
              if (page['x-t']) S.call(self, 'X-t', page['x-t']);
              if (hasPageCommon) S.call(self, 'X-s-common', page['x-s-common']);
              notePr('passthrough', { pageCommon: hasPageCommon, pageXt: !!page['x-t'] });
              self.addEventListener('loadend', function () {
                try { if (stats.pr_last) { stats.pr_last.status = self.status; } } catch (e) {}
              });
              bump('passthrough');
              if (hasPageCommon) bump('passthrough_coherent');
              return D.apply(self, args);
            }
            // ELSE resign (same-origin unsigned APIs, or anything the page signed;
            // cross-origin unsigned must stay untouched — extra x-s = preflight break).
            if (sg && (!cross || hasPageXs)) {
              // Ark poison path: page had x-s-common (fU ok) but fy failed (no
              // mnsv2), so we must not attach www XYW_ and strip common.
              if (snap.pr && (hasPageCommon || isArkHost()) && isXywPoison(sg)) {
                bump('xyw_blocked');
                notePr('blocked', { reason: 'xyw-no-mnsv2', pageCommon: hasPageCommon, fam: sgFamily(sg) });
                bump('pr_blocked');
                try {
                  Object.defineProperty(self, 'readyState', { value: 4, configurable: true });
                  Object.defineProperty(self, 'status', { value: 200, configurable: true });
                  Object.defineProperty(self, 'responseText', {
                    value: JSON.stringify({ code: -1, success: false, msg: 'xs-gate:awaiting-mnsv2-xys' }),
                    configurable: true
                  });
                  setTimeout(() => { try {
                    self.dispatchEvent(new Event('readystatechange'));
                    self.dispatchEvent(new Event('load'));
                    self.dispatchEvent(new Event('loadend'));
                  } catch (e) {} }, 0);
                  return;
                } catch (e) { return; }
              }
              S.call(self, 'X-s', String(sg['X-s']));
              S.call(self, 'X-t', String(sg['X-t']));
              const cm = commonFromSg(sg);
              const fam = sgFamily(sg);
              // Ark XYS_ pairs with page fU common (signer does not mint common).
              if (cm) {
                S.call(self, 'X-s-common', cm);
              } else if (fam.prefix === 'XYS_' && (hasPageCommon || snap.deferred) && page['x-s-common']) {
                S.call(self, 'X-s-common', page['x-s-common']);
              } else if (self.__xs_common_deferred || hasPageCommon) {
                if (fam.prefix === 'XYS_' && page['x-s-common']) {
                  S.call(self, 'X-s-common', page['x-s-common']);
                } else {
                  bump('common_stripped');
                  // intentionally omit page x-s-common — never pair stale common with www X-s/X-t
                }
              }
              notePr('resign', { signerCommon: !!cm, pageCommon: hasPageCommon, fam: fam });
              self.addEventListener('loadend', function () {
                try { if (stats.pr_last) { stats.pr_last.status = self.status; } } catch (e) {}
              });
              bump('signed');
            } else if (snap.pr && !allowUnsigned) {
              // Do not wire unsigned publish_render — synthetic soft fail (avoid 300015 poison).
              // Covers FormData bodies too: the gate must hold regardless of body type.
              notePr('blocked', { body: formBody ? 'FormData' : 'other' });
              bump('pr_blocked');
              try {
                Object.defineProperty(self, 'readyState', { value: 4, configurable: true });
                Object.defineProperty(self, 'status', { value: 200, configurable: true });
                Object.defineProperty(self, 'responseText', {
                  value: JSON.stringify({ code: -1, success: false, msg: 'xs-gate:signer-not-ready' }),
                  configurable: true
                });
                setTimeout(() => { try {
                  self.dispatchEvent(new Event('readystatechange'));
                  self.dispatchEvent(new Event('load'));
                  self.dispatchEvent(new Event('loadend'));
                } catch (e) {} }, 0);
                return;
              } catch (e) { return; }
            } else if (hasPageXs && !formBody) {
              // Resign failed (no signer) but we swallowed the page's own x-s at
              // setRequestHeader — restore it instead of sending bare (a page-
              // signed request would otherwise leave unsigned = 406).
              S.call(self, 'X-s', page['x-s']);
              if (page['x-t']) S.call(self, 'X-t', page['x-t']);
              if (hasPageCommon) S.call(self, 'X-s-common', page['x-s-common']);
              if (snap.pr) notePr('replayed');
              bump('replayed');
              return D.apply(self, args);
            } else if (snap.deferred && hasPageCommon && !cross) {
              // Unsigned same-origin request whose only page header was the
              // deferred x-s-common — put it back; we neither signed nor
              // passthrough'd, the page's original intent stands.
              S.call(self, 'X-s-common', page['x-s-common']);
              return D.apply(self, args);
            }
          }
        } catch (e) {}
        return D.apply(self, args);
      };
      try {
        const u = snap.u;
        if (snap.pr || isPublishRender(u)) {
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
          if (gated) notePr('passthrough', { via: 'fetch', pageCommon: hasCommon });
          bump('passthrough');
          if (hasCommon) bump('passthrough_coherent');
          return F.call(this, u, o);
        }
        if (!sg) {
          if (gated) {
            notePr('blocked', { via: 'fetch' });
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
        if (gated && (hasCommon || isArkHost()) && isXywPoison(sg)) {
          notePr('blocked', { via: 'fetch', reason: 'xyw-no-mnsv2', pageCommon: hasCommon, fam: sgFamily(sg) });
          bump('xyw_blocked');
          bump('pr_blocked');
          return Promise.resolve(new Response(
            JSON.stringify({ code: -1, success: false, msg: 'xs-gate:awaiting-mnsv2-xys' }),
            { status: 200, headers: { 'content-type': 'application/json' } }
          ));
        }
        const cm = commonFromSg(sg);
        const famF = sgFamily(sg);
        // Capture page common before rebuild (XYS_ pairs with fU common).
        let pageCommonVal = null;
        try {
          if (h instanceof Headers) pageCommonVal = h.get('x-s-common') || h.get('X-s-common');
          else if (h && !Array.isArray(h)) {
            pageCommonVal = h['x-s-common'] || h['X-s-common'];
            if (!pageCommonVal) {
              for (const k of Object.keys(h)) if (String(k).toLowerCase() === 'x-s-common') { pageCommonVal = h[k]; break; }
            }
          } else if (Array.isArray(h)) {
            for (const pair of h) {
              if (pair && pair.length >= 2 && String(pair[0]).toLowerCase() === 'x-s-common') {
                pageCommonVal = String(pair[1]); break;
              }
            }
          }
        } catch (e) {}
        const pickCommon = () => {
          if (cm) return cm;
          if (famF.prefix === 'XYS_' && pageCommonVal) return pageCommonVal;
          return null;
        };
        const outCm = pickCommon();
        let nh;
        if (!h || Array.isArray(h)) {
          // Array-of-pairs form: rebuild via Headers so page headers survive
          // (dropping content-type etc. turned JSON POSTs into bare 4xx).
          nh = new Headers();
          if (Array.isArray(h)) {
            for (const pair of h) {
              if (pair && pair.length >= 2) {
                const k = String(pair[0]);
                if (!/^x-(s|t|s-common)$/i.test(k.trim())) nh.append(k, String(pair[1]));
              }
            }
          }
          nh.set('x-s', String(sg['X-s'])); nh.set('x-t', String(sg['X-t']));
          if (outCm) nh.set('x-s-common', outCm);
          else if (hasCommon) bump('common_stripped');
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
          if (outCm) nh['x-s-common'] = outCm;
          else if (hasCommon) bump('common_stripped');
        } else {
          nh = new Headers(h);
          nh.delete('x-s'); nh.delete('X-s'); nh.delete('x-t'); nh.delete('X-t');
          nh.delete('x-s-common'); nh.delete('X-s-common');
          nh.set('x-s', String(sg['X-s'])); nh.set('x-t', String(sg['X-t']));
          if (outCm) nh.set('x-s-common', outCm);
          else if (hasCommon) bump('common_stripped');
        }
        if (hasXs) bump('swallowed');
        bump('signed');
        if (gated) notePr('resign', { via: 'fetch', signerCommon: !!cm, pageCommon: hasCommon, fam: famF, keptPageCommon: !!(outCm && !cm) });
        return F.call(this, u, Object.assign({}, o, { headers: nh }));
      };
      if (!isXhsUrl(us) && !isPublishRender(us)) return F.call(this, u, o);
      const has = headerHas(o.headers, ['x-s', 'X-s']);
      const gated = isPublishRender(us);
      // Resign when page set x-s; also auto-sign /api/* (ark publish_render omits x-s
      // when window._webmsxyw is missing) — but never auto-sign a CROSS-origin URL
      // the page left unsigned (extra x-s forces a preflight the server rejects).
      // Skip non-API without x-s (unless gated).
      if (!gated && !has && (!isApiUrl(us) || !sameOrigin(us))) return F.call(this, u, o);
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
      if (!has && (!isApiUrl(us) || !sameOrigin(us))) return apply(null, false);
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
  return 'patched-v11-wasm-guard';
})()
