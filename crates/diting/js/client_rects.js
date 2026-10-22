// Range.getClientRects / getBoundingClientRect and per-line Element.getClientRects
// (split from bootstrap.js). The geometry comes from the native text_rects /
// elem_rects ops, which re-wrap the painted text with the painter's own line
// breaker, so the boxes sit exactly where the glyphs are drawn. Call-time only.
(() => {
  const scroll = () => {
    const root = globalThis.document && globalThis.document.documentElement;
    return [(root && root._scrollLeft) || 0, (root && root._scrollTop) || 0];
  };
  const rect = (x, y, w, h) => (typeof DOMRect === 'function' ? new DOMRect(x, y, w, h) : { x, y, width: w, height: h, top: y, left: x, right: x + w, bottom: y + h });
  const ask = (cmd, nid, arg) => {
    try {
      const raw = _domRaw(cmd, String(nid), arg || '');
      const arr = typeof raw === 'string' ? JSON.parse(raw) : raw;
      return Array.isArray(arr) ? arr : null;
    } catch (e) { return null; }
  };
  const toClient = (arr) => {
    const [sx, sy] = scroll();
    return arr.map((r) => rect(r[0] - sx, r[1] - sy, r[2], r[3]));
  };
  const list = (rects) => {
    Object.defineProperty(rects, 'item', { value(i) { return rects[i] || null; }, configurable: true, writable: true });
    return rects;
  };
  const textLen = (n) => (n.data || n.nodeValue || '').length;
  const before = (n, o, m, p) => _rngCmp(n, o, m, p) <= 0;

  // ---- Element.getClientRects: one box per line for a wrapped inline ----
  const origEl = Element.prototype.getClientRects;
  Object.defineProperty(Element.prototype, 'getClientRects', {
    value: function getClientRects() {
      if (this._nid != null && this.isConnected) {
        const lines = ask('elem_rects', this._nid);
        if (lines && lines.length) return list(toClient(lines));
      }
      return list(origEl.call(this));
    },
    writable: true, configurable: true,
  });
  _markNative(Element.prototype.getClientRects);

  // ---- Range rects ------------------------------------------------------
  const rangeRects = (range) => {
    const out = [];
    const sc = range.startContainer, so = range.startOffset, ec = range.endContainer, eo = range.endOffset;
    if (!sc || !ec) return out;
    const root = range.commonAncestorContainer;
    const contained = (n) => {
      const p = n.parentNode;
      if (!p) return false;
      const i = _rngNodeIndex(n);
      return before(sc, so, p, i) && before(p, i + 1, ec, eo);
    };
    const walk = (n) => {
      if (n.nodeType === 3 || n.nodeType === 4) {
        let from = n === sc ? so : 0;
        let to = n === ec ? eo : textLen(n);
        if (n !== sc && !before(sc, so, n, 0)) return;
        if (n !== ec && !before(n, textLen(n), ec, eo)) return;
        if (range.collapsed && n !== sc) return;
        const rects = ask('text_rects', n._nid, from + ',' + to);
        if (rects) out.push(...toClient(rects));
        return;
      }
      if (n.nodeType === 1 && n !== root && contained(n) && !(n.parentNode && n.parentNode !== root && contained(n.parentNode))) {
        const rs = n.getClientRects();
        for (const r of rs) out.push(r);
      }
      for (const c of n.childNodes) walk(c);
    };
    if (range.collapsed) {
      if (sc.nodeType === 3) walk(sc);
      return out;
    }
    if (root.nodeType === 3) walk(root);
    else for (const c of root.childNodes) walk(c);
    return out;
  };
  const def = (n, f) => { Object.defineProperty(Range.prototype, n, { value: f, writable: true, configurable: true }); _markNative(Range.prototype[n]); };
  def('getClientRects', function getClientRects() { return list(rangeRects(this)); });
  def('getBoundingClientRect', function getBoundingClientRect() {
    const rs = rangeRects(this);
    if (!rs.length) return rect(0, 0, 0, 0);
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const r of rs) { x0 = Math.min(x0, r.x); y0 = Math.min(y0, r.y); x1 = Math.max(x1, r.x + r.width); y1 = Math.max(y1, r.y + r.height); }
    return rect(x0, y0, x1 - x0, y1 - y0);
  });
})();
