// Range / StaticRange / Selection.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.

// Live Range over the real DOM tree. dom/ranges/* tests are pure boundary-point
// algorithms (no layout, no editing engine), so a property-storing Range with
// correct tree-order comparison passes them. Mutating ops (extract/delete/
// insert/surround) are kept minimal: they do not throw, but do not rewrite the
// tree (that is the editing mega-bucket, out of scope).
function _rngNodeLength(n) {
  const t = n.nodeType;
  if (t === 3 || t === 4 || t === 8 || t === 7) return (n.data || n.nodeValue || "").length;
  return n.childNodes.length;
}
// Index among siblings, computed in Rust (one op) instead of serializing the
// whole childNodes list per call: the Range matrices call this heavily.
function _rngNodeIndex(n) {
  if (!n.parentNode) return 0;
  return +_dom("node_index", n._nid);
}
function _rngSame(a, b) { return a === b || (!!a && !!b && a._nid === b._nid); }
// Root nid in one op (callers only read ._nid), instead of an O(depth) walk.
function _rngRoot(n) { return { _nid: +_dom("node_root", n._nid) }; }
function _rngAncestors(n) { const a = []; let c = n; while (c) { a.push(c); c = c.parentNode; } return a; }
// document (preorder) tree order: -1 if a precedes b, 1 if a follows b, 0 same.
// Computed in Rust (one op) rather than walking ancestor chains over per-step
// DOM ops, which made the large dom/ranges matrices time out.
function _rngOrder(a, b) {
  if (_rngSame(a, b)) return 0;
  return +_dom("compare_order", a._nid, b._nid) || 0;
}
// Position of (nA,oA) relative to (nB,oB): -1 before, 0 equal, 1 after.
function _rngCmp(nA, oA, nB, oB) {
  if (_rngSame(nA, nB)) return oA < oB ? -1 : (oA > oB ? 1 : 0);
  if (_rngOrder(nA, nB) > 0) return -_rngCmp(nB, oB, nA, oA);
  if (nA.contains && nA.contains(nB)) { // nA is a strict ancestor of nB
    let child = nB;
    while (child && child.parentNode && child.parentNode._nid !== nA._nid) child = child.parentNode;
    if (child && child.parentNode && child.parentNode._nid === nA._nid && _rngNodeIndex(child) < oA) return 1;
    return -1;
  }
  return -1;
}
function _rngCheckOffset(n, o) {
  if (n && n.nodeType === 10) throw new DOMException("Range boundary cannot be a DocumentType", "InvalidNodeTypeError");
  if (o < 0 || o > _rngNodeLength(n)) throw new DOMException("Range offset out of bounds", "IndexSizeError");
}
globalThis.Range = class Range {
  constructor() {
    const d = globalThis.document || null;
    this._sc = d; this._so = 0; this._ec = d; this._eo = 0;
    __hideOwn(this);
  }
  get startContainer() { return this._sc; }
  get startOffset() { return this._so; }
  get endContainer() { return this._ec; }
  get endOffset() { return this._eo; }
  get collapsed() { return _rngSame(this._sc, this._ec) && this._so === this._eo; }
  get commonAncestorContainer() {
    if (!this._sc || !this._ec) return null;
    const setA = new Set(_rngAncestors(this._sc).map(n => n._nid));
    let c = this._ec;
    while (c) { if (setA.has(c._nid)) return c; c = c.parentNode; }
    return null;
  }
  setStart(n, o) { _rngCheckOffset(n, o); this._sc = n; this._so = o; if (_rngRoot(n)._nid !== _rngRoot(this._ec)._nid || _rngCmp(this._sc, this._so, this._ec, this._eo) > 0) { this._ec = n; this._eo = o; } }
  setEnd(n, o) { _rngCheckOffset(n, o); this._ec = n; this._eo = o; if (_rngRoot(n)._nid !== _rngRoot(this._sc)._nid || _rngCmp(this._sc, this._so, this._ec, this._eo) > 0) { this._sc = n; this._so = o; } }
  setStartBefore(n) { const p = n.parentNode; if (!p) throw new DOMException("node has no parent", "InvalidNodeTypeError"); this.setStart(p, _rngNodeIndex(n)); }
  setStartAfter(n) { const p = n.parentNode; if (!p) throw new DOMException("node has no parent", "InvalidNodeTypeError"); this.setStart(p, _rngNodeIndex(n) + 1); }
  setEndBefore(n) { const p = n.parentNode; if (!p) throw new DOMException("node has no parent", "InvalidNodeTypeError"); this.setEnd(p, _rngNodeIndex(n)); }
  setEndAfter(n) { const p = n.parentNode; if (!p) throw new DOMException("node has no parent", "InvalidNodeTypeError"); this.setEnd(p, _rngNodeIndex(n) + 1); }
  collapse(toStart) { if (toStart) { this._ec = this._sc; this._eo = this._so; } else { this._sc = this._ec; this._so = this._eo; } }
  selectNode(n) { const p = n.parentNode; if (!p) throw new DOMException("node has no parent", "InvalidNodeTypeError"); const i = _rngNodeIndex(n); this._sc = p; this._so = i; this._ec = p; this._eo = i + 1; }
  selectNodeContents(n) { if (n && n.nodeType === 10) throw new DOMException("cannot select a DocumentType", "InvalidNodeTypeError"); const len = _rngNodeLength(n); this._sc = n; this._so = 0; this._ec = n; this._eo = len; }
  comparePoint(n, o) {
    o = o >>> 0; // offset is a WebIDL unsigned long: -1 -> 4294967295 -> IndexSizeError
    if (_rngRoot(n)._nid !== _rngRoot(this._sc)._nid) throw new DOMException("nodes are in different trees", "WrongDocumentError");
    if (n.nodeType === 10) throw new DOMException("node is a DocumentType", "InvalidNodeTypeError");
    if (o > _rngNodeLength(n)) throw new DOMException("offset out of bounds", "IndexSizeError");
    if (_rngCmp(n, o, this._sc, this._so) < 0) return -1;
    if (_rngCmp(n, o, this._ec, this._eo) > 0) return 1;
    return 0;
  }
  isPointInRange(n, o) {
    o = o >>> 0;
    if (!this._sc || _rngRoot(n)._nid !== _rngRoot(this._sc)._nid) return false;
    if (n.nodeType === 10) throw new DOMException("node is a DocumentType", "InvalidNodeTypeError");
    if (o > _rngNodeLength(n)) throw new DOMException("offset out of bounds", "IndexSizeError");
    return _rngCmp(n, o, this._sc, this._so) >= 0 && _rngCmp(n, o, this._ec, this._eo) <= 0;
  }
  compareBoundaryPoints(how, other) {
    // `how` is a WebIDL `unsigned short`: ToUint16-convert before validating,
    // so NaN/Infinity become 0 (START_TO_START) rather than throwing.
    let h = Math.trunc(Number(how));
    if (!Number.isFinite(h)) h = 0;
    h = ((h % 65536) + 65536) % 65536;
    let a, b;
    switch (h) {
      case 0: a = [this._sc, this._so]; b = [other._sc, other._so]; break; // START_TO_START
      case 1: a = [this._ec, this._eo]; b = [other._sc, other._so]; break; // START_TO_END
      case 2: a = [this._ec, this._eo]; b = [other._ec, other._eo]; break; // END_TO_END
      case 3: a = [this._sc, this._so]; b = [other._ec, other._eo]; break; // END_TO_START
      default: throw new DOMException("invalid comparison type", "NotSupportedError");
    }
    // Different roots -> WrongDocumentError. Guard so a null/foreign container
    // raises that DOMException rather than a raw TypeError from _rngRoot.
    let differ;
    try { differ = _rngRoot(a[0])._nid !== _rngRoot(b[0])._nid; }
    catch (e) { differ = true; }
    if (differ) throw new DOMException("The two Ranges are not in the same tree.", "WrongDocumentError");
    return _rngCmp(a[0], a[1], b[0], b[1]);
  }
  intersectsNode(n) {
    if (_rngRoot(n)._nid !== _rngRoot(this._sc)._nid) return false;
    const p = n.parentNode;
    if (!p) return true;
    const o = _rngNodeIndex(n);
    return _rngCmp(p, o, this._ec, this._eo) < 0 && _rngCmp(p, o + 1, this._sc, this._so) > 0;
  }
  cloneRange() { const r = new Range(); r._sc = this._sc; r._so = this._so; r._ec = this._ec; r._eo = this._eo; return r; }
  createContextualFragment(html) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'createContextualFragment' on 'Range': 1 argument required, but only 0 present.");
    const node = this._sc;
    const ownerDoc = (node && node.ownerDocument) || globalThis.document;
    const frag = ownerDoc.createDocumentFragment();
    frag.innerHTML = String(html);
    return frag;
  }
  toString() {
    const sc = this._sc, ec = this._ec;
    if (!sc) return "";
    if (_rngSame(sc, ec) && (sc.nodeType === 3 || sc.nodeType === 4)) return (sc.data || "").slice(this._so, this._eo);
    let s = "";
    if (sc.nodeType === 3 || sc.nodeType === 4) s += (sc.data || "").slice(this._so);
    const cac = this.commonAncestorContainer;
    if (cac) {
      const walk = (node) => {
        if (node.nodeType === 3 || node.nodeType === 4) {
          if (!_rngSame(node, sc) && !_rngSame(node, ec) &&
              _rngCmp(node, 0, this._sc, this._so) >= 0 && _rngCmp(node, _rngNodeLength(node), this._ec, this._eo) <= 0) {
            s += (node.data || "");
          }
        }
        const kids = node.childNodes;
        for (let i = 0; i < kids.length; i++) if (kids[i]) walk(kids[i]);
      };
      walk(cac);
    }
    if (!_rngSame(sc, ec) && (ec.nodeType === 3 || ec.nodeType === 4)) s += (ec.data || "").slice(0, this._eo);
    return s;
  }
  cloneContents() { return (globalThis.document || document).createDocumentFragment(); }
  extractContents() { return (globalThis.document || document).createDocumentFragment(); }
  deleteContents() {}
  insertNode(node) { if (node && this._sc && this._sc.insertBefore) { const kids = this._sc.childNodes; this._sc.insertBefore(node, kids[this._so] || null); } }
  surroundContents(node) { this.insertNode(node); }
  detach() {}
  getBoundingClientRect() { return new DOMRect(); }
  getClientRects() { return []; }
  static get START_TO_START() { return 0; }
  static get START_TO_END() { return 1; }
  static get END_TO_END() { return 2; }
  static get END_TO_START() { return 3; }
};
Object.assign(globalThis.Range.prototype, { START_TO_START: 0, START_TO_END: 1, END_TO_END: 2, END_TO_START: 3 });
globalThis.StaticRange = class StaticRange {
  constructor(init) {
    if (!init || init.startContainer == null || init.endContainer == null)
      throw new TypeError("Failed to construct 'StaticRange': required members are undefined");
    const sc = init.startContainer, ec = init.endContainer;
    if (sc.nodeType === 10 || ec.nodeType === 10 || sc.nodeType === 7 || ec.nodeType === 7)
      throw new DOMException("StaticRange endpoints cannot be DocumentType or ProcessingInstruction", "InvalidNodeTypeError");
    this._sc = sc; this._so = init.startOffset >>> 0; this._ec = ec; this._eo = init.endOffset >>> 0;
    __hideOwn(this);
  }
  get startContainer() { return this._sc; }
  get startOffset() { return this._so; }
  get endContainer() { return this._ec; }
  get endOffset() { return this._eo; }
  get collapsed() { return _rngSame(this._sc, this._ec) && this._so === this._eo; }
};
// Live Selection over the real Range: at most one range + a direction, one
// instance per document. Everything except modify() (needs visual line/word
// layout) is layout-free, built on the Range boundary-point helpers above.
globalThis.Selection = class Selection {
  constructor(doc) { this._doc = doc; this._range = null; this._direction = 'none';  __hideOwn(this);}
  _setRange(r, dir) { this._range = r; this._direction = dir; }
  _inDoc(node) { return !!(node && this._doc && this._doc.contains && this._doc.contains(node)); }
  get rangeCount() { return this._range ? 1 : 0; }
  get isCollapsed() { return !this._range || this._range.collapsed; }
  get type() { return !this._range ? 'None' : (this._range.collapsed ? 'Caret' : 'Range'); }
  get _anchor() { const r = this._range; if (!r) return null; return this._direction === 'backwards' ? [r.endContainer, r.endOffset] : [r.startContainer, r.startOffset]; }
  get _focus() { const r = this._range; if (!r) return null; return this._direction === 'backwards' ? [r.startContainer, r.startOffset] : [r.endContainer, r.endOffset]; }
  get anchorNode() { return this._anchor ? this._anchor[0] : null; }
  get anchorOffset() { return this._anchor ? this._anchor[1] : 0; }
  get focusNode() { return this._focus ? this._focus[0] : null; }
  get focusOffset() { return this._focus ? this._focus[1] : 0; }
  getRangeAt(i) { i = +i; if (!this._range || i < 0 || i > 0) throw new DOMException('The index provided is out of range.', 'IndexSizeError'); return this._range; }
  addRange(range) { if (this._range) return; if (!(range instanceof Range)) return; if (!this._inDoc(range.startContainer) || !this._inDoc(range.endContainer)) return; this._setRange(range, 'forwards'); }
  removeRange(range) { if (!(range instanceof Range)) throw new TypeError("Failed to execute 'removeRange' on 'Selection': parameter 1 is not a Range."); if (this._range === range) this._setRange(null, 'none'); else throw new DOMException('The range was not found.', 'NotFoundError'); }
  removeAllRanges() { this._setRange(null, 'none'); }
  empty() { this.removeAllRanges(); }
  collapse(node, offset) { if (node == null) { this.removeAllRanges(); return; } offset = offset >>> 0; _rngCheckOffset(node, offset); if (!this._inDoc(node)) return; const r = new Range(); r.setStart(node, offset); r.setEnd(node, offset); this._setRange(r, 'forwards'); }
  setPosition(node, offset) { this.collapse(node, offset); }
  collapseToStart() { if (!this._range) throw new DOMException('There is no selection to collapse.', 'InvalidStateError'); const r = new Range(); r.setStart(this._range.startContainer, this._range.startOffset); r.setEnd(this._range.startContainer, this._range.startOffset); this._setRange(r, 'forwards'); }
  collapseToEnd() { if (!this._range) throw new DOMException('There is no selection to collapse.', 'InvalidStateError'); const r = new Range(); r.setStart(this._range.endContainer, this._range.endOffset); r.setEnd(this._range.endContainer, this._range.endOffset); this._setRange(r, 'forwards'); }
  extend(node, offset) { if (!this._range) throw new DOMException('There is no selection to extend.', 'InvalidStateError'); if (!this._inDoc(node)) return; offset = offset >>> 0; _rngCheckOffset(node, offset); const a = this._anchor; const r = new Range(); if (_rngRoot(node)._nid !== _rngRoot(a[0])._nid) { r.setStart(node, offset); r.setEnd(node, offset); this._setRange(r, 'forwards'); return; } if (_rngCmp(a[0], a[1], node, offset) <= 0) { r.setStart(a[0], a[1]); r.setEnd(node, offset); this._setRange(r, 'forwards'); } else { r.setStart(node, offset); r.setEnd(a[0], a[1]); this._setRange(r, 'backwards'); } }
  setBaseAndExtent(aN, aO, fN, fO) { if (arguments.length < 4) throw new TypeError("Failed to execute 'setBaseAndExtent' on 'Selection': 4 arguments required."); if (aN == null || fN == null) throw new TypeError("Failed to execute 'setBaseAndExtent' on 'Selection': nodes must not be null."); aO = +aO; fO = +fO; if (aO < 0 || aO > _rngNodeLength(aN)) throw new DOMException('anchor offset out of range', 'IndexSizeError'); if (fO < 0 || fO > _rngNodeLength(fN)) throw new DOMException('focus offset out of range', 'IndexSizeError'); if (!this._inDoc(aN) || !this._inDoc(fN)) { this.removeAllRanges(); return; } const r = new Range(); if (_rngCmp(aN, aO, fN, fO) <= 0) { r.setStart(aN, aO); r.setEnd(fN, fO); this._setRange(r, 'forwards'); } else { r.setStart(fN, fO); r.setEnd(aN, aO); this._setRange(r, 'backwards'); } }
  selectAllChildren(node) { if (node && node.nodeType === 10) throw new DOMException('cannot selectAllChildren of a DocumentType', 'InvalidNodeTypeError'); if (!this._inDoc(node)) return; const len = _rngNodeLength(node); const r = new Range(); r.setStart(node, 0); r.setEnd(node, len); this._setRange(r, 'forwards'); }
  containsNode(node, allowPartial) { const r = this._range; if (!r || !node) return false; if (_rngRoot(node)._nid !== _rngRoot(r.startContainer)._nid) return false; const len = _rngNodeLength(node); if (allowPartial) return _rngCmp(node, len, r.startContainer, r.startOffset) > 0 && _rngCmp(node, 0, r.endContainer, r.endOffset) < 0; return _rngCmp(node, 0, r.startContainer, r.startOffset) >= 0 && _rngCmp(node, len, r.endContainer, r.endOffset) <= 0; }
  deleteFromDocument() { if (this._range) this._range.deleteContents(); }
  toString() { return this._range ? this._range.toString() : ''; }
  modify() {}
};
_markNative(globalThis.Selection);
