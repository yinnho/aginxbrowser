// CSSOM rule tree (split from bootstrap.js): CSSStyleSheet.cssRules holds real
// CSSRule objects — CSSStyleRule, CSSMediaRule, CSSSupportsRule, CSSContainerRule,
// CSSLayerBlockRule/StatementRule, CSSImportRule, CSSFontFaceRule, CSSKeyframes-
// Rule/CSSKeyframeRule, CSSPageRule, CSSNamespaceRule, CSSPropertyRule, … —
// parsed from the sheet text, with Chrome's cssText serialisation, nested
// cssRules, insertRule/deleteRule at every level, and write-through of edits to
// <style>-owned sheets into the cascade (CSS-in-JS libraries insert rules this
// way). Everything is call-time.
(() => {
  const D = new WeakMap(); // rule -> internal record
  const rec = (r) => D.get(r);

  // ---- scanning ---------------------------------------------------------
  // Top-level items of a rule list or declaration block:
  //   {block:false, head}              a `;`-terminated statement / declaration
  //   {block:true, head, body}         `head { body }`
  const scan = (text) => {
    const items = [];
    let i = 0, start = 0, paren = 0, quote = '';
    const n = text.length;
    const flushStmt = (end) => { const h = text.slice(start, end).trim(); if (h) items.push({ block: false, head: h }); };
    while (i < n) {
      const c = text[i];
      if (quote) {
        if (c === '\\') i++; else if (c === quote) quote = '';
      } else if (c === '/' && text[i + 1] === '*') {
        const e = text.indexOf('*/', i + 2);
        text = text.slice(0, i) + (e < 0 ? '' : text.slice(e + 2));
        continue;
      } else if (c === '"' || c === "'") quote = c;
      else if (c === '(') paren++;
      else if (c === ')') paren--;
      else if (c === ';' && paren <= 0) { flushStmt(i); start = i + 1; }
      else if (c === '{' && paren <= 0) {
        let depth = 1, j = i + 1, q = '';
        while (j < n && depth) {
          const d = text[j];
          if (q) { if (d === '\\') j++; else if (d === q) q = ''; }
          else if (d === '"' || d === "'") q = d;
          else if (d === '{') depth++;
          else if (d === '}') depth--;
          j++;
        }
        items.push({ block: true, head: text.slice(start, i).trim(), body: text.slice(i + 1, depth ? n : j - 1) });
        start = j; i = j; continue;
      }
      i++;
    }
    flushStmt(n);
    return items;
  };

  // ---- normalisation ----------------------------------------------------
  const splitTop = (s, sep) => {
    const out = []; let cur = '', p = 0, q = '';
    for (const c of s) {
      if (q) { cur += c; if (c === q) q = ''; continue; }
      if (c === '"' || c === "'") { q = c; cur += c; continue; }
      if (c === '(' || c === '[') p++; else if (c === ')' || c === ']') p--;
      if (c === sep && p === 0) { out.push(cur); cur = ''; } else cur += c;
    }
    out.push(cur);
    return out;
  };
  const normSelector = (s) => splitTop(s, ',').map((x) => x.trim().replace(/\s+/g, ' ')).join(', ');
  const normCondition = (s) => s.trim().replace(/\s+/g, ' ').replace(/\(\s*([\w-]+)\s*:\s*/g, '($1: ').replace(/\s*\)/g, ')').replace(/\bAND\b/g, 'and');
  const normKeyText = (s) => splitTop(s, ',').map((x) => { const t = x.trim().toLowerCase(); return t === 'from' ? '0%' : t === 'to' ? '100%' : t; }).join(', ');

  const decl = (text) => { const s = _styleProxy(new CSSStyleDeclaration(null)); s.cssText = text; return s; };
  // Chrome serialises url() arguments quoted.
  const declText = (s) => s.cssText.replace(/url\(\s*([^"')\s][^)]*?)\s*\)/g, 'url("$1")');

  // ---- rule construction ------------------------------------------------
  const GROUP = new Set(['media', 'supports', 'container', 'layer', 'scope', 'starting-style', 'document']);
  const mk = (clsName, data) => {
    const C = globalThis[clsName] || globalThis.CSSRule;
    const r = Object.create(C.prototype);
    D.set(r, data);
    return r;
  };
  const list = (rules) => {
    Object.defineProperty(rules, 'item', { value(i) { return rules[i] || null; }, configurable: true, writable: true });
    return rules;
  };

  const build = (item, parent, sheet) => {
    const head = item.head;
    if (!head) return null;
    if (head[0] !== '@') {
      if (!item.block) return null;
      const r = mk('CSSStyleRule', { kind: 'style', selector: normSelector(head), parent, sheet, children: list([]), style: null, decl: '' });
      const inner = scan(item.body);
      const own = inner.filter((x) => !x.block).map((x) => x.head).join('; ');
      rec(r).style = decl(own);
      for (const x of inner) if (x.block) { const c = build(x, r, sheet); if (c) rec(r).children.push(c); }
      return r;
    }
    const m = /^@([\w-]+)\s*([\s\S]*)$/.exec(head);
    if (!m) return null;
    const name = m[1].toLowerCase().replace(/^-(webkit|moz)-/, '');
    const rest = m[2].trim();
    if (!item.block) {
      if (name === 'import') return mk('CSSImportRule', { kind: 'import', href: (/url\(\s*["']?([^"')]+)["']?\s*\)|["']([^"']+)["']/.exec(rest) || [])[1] || (/["']([^"']+)["']/.exec(rest) || [])[1] || '', rest, parent, sheet });
      if (name === 'charset') return null;
      if (name === 'namespace') { const mm = /^(?:([\w-]+)\s+)?(?:url\(\s*["']?([^"')]+)["']?\s*\)|["']([^"']+)["'])/.exec(rest) || []; return mk('CSSNamespaceRule', { kind: 'namespace', prefix: mm[1] || '', uri: mm[2] || mm[3] || '', parent, sheet }); }
      if (name === 'layer') return mk('CSSLayerStatementRule', { kind: 'layerstmt', names: splitTop(rest, ',').map((x) => x.trim()).filter(Boolean), parent, sheet });
      return null;
    }
    if (GROUP.has(name)) {
      const cls = { media: 'CSSMediaRule', supports: 'CSSSupportsRule', container: 'CSSContainerRule', layer: 'CSSLayerBlockRule', scope: 'CSSScopeRule', 'starting-style': 'CSSStartingStyleRule' }[name] || 'CSSRule';
      const r = mk(cls, { kind: name, cond: name === 'layer' || name === 'starting-style' ? '' : (name === 'supports' ? rest.replace(/\s+/g, ' ') : normCondition(rest)), name: name === 'layer' ? rest : '', parent, sheet, children: list([]) });
      for (const x of scan(item.body)) { const c = build(x, r, sheet); if (c) rec(r).children.push(c); }
      return r;
    }
    if (name === 'font-face' || name === 'page' || name === 'property' || name === 'counter-style' || name === 'font-feature-values' || name === 'position-try' || name === 'font-palette-values' || name === 'view-transition') {
      const cls = { 'font-face': 'CSSFontFaceRule', page: 'CSSPageRule', property: 'CSSPropertyRule', 'counter-style': 'CSSCounterStyleRule', 'position-try': 'CSSPositionTryRule', 'font-palette-values': 'CSSFontPaletteValuesRule' }[name] || 'CSSRule';
      return mk(cls, { kind: name, prelude: rest, parent, sheet, style: decl(scan(item.body).filter((x) => !x.block).map((x) => x.head).join('; ')) });
    }
    if (name === 'keyframes') {
      const r = mk('CSSKeyframesRule', { kind: 'keyframes', name: rest.replace(/^["']|["']$/g, ''), parent, sheet, children: list([]) });
      for (const x of scan(item.body)) {
        if (!x.block) continue;
        const k = mk('CSSKeyframeRule', { kind: 'keyframe', keyText: normKeyText(x.head), parent: r, sheet, style: decl(scan(x.body).filter((y) => !y.block).map((y) => y.head).join('; ')) });
        rec(r).children.push(k);
      }
      return r;
    }
    return null;
  };

  // ---- serialisation (Chrome's spelling) --------------------------------
  const ser = (r) => {
    const d = rec(r);
    const ind = (t) => t.split('\n').map((l) => '  ' + l).join('\n');
    const kids = (x) => x.children.map((k) => ind(ser(k))).join('\n');
    const group = (head, x) => (x.children.length ? `${head} {\n${kids(x)}\n}` : `${head} {\n}`);
    switch (d.kind) {
      case 'style': {
        const own = declText(d.style);
        if (!d.children.length) return own ? `${d.selector} { ${own} }` : `${d.selector} { }`;
        return `${d.selector} {\n${own ? ind(own) + '\n' : ''}${kids(d)}\n}`;
      }
      case 'media': return group('@media ' + d.cond, d);
      case 'supports': return group('@supports ' + d.cond, d);
      case 'container': return group('@container ' + d.cond, d);
      case 'scope': return group('@scope ' + d.cond, d);
      case 'starting-style': return group('@starting-style', d);
      case 'layer': return group('@layer' + (d.name ? ' ' + d.name : ''), d);
      case 'layerstmt': return '@layer ' + d.names.join(', ') + ';';
      case 'import': return '@import ' + d.rest + ';';
      case 'charset': return '@charset ' + d.rest + ';';
      case 'namespace': return '@namespace ' + (d.prefix ? d.prefix + ' ' : '') + 'url("' + d.uri + '");';
      case 'keyframes': return `@keyframes ${d.name} { \n${kids(d)}\n}`;
      case 'keyframe': return `${d.keyText} { ${declText(d.style)} }`;
      default: return `@${d.kind}${d.prelude ? ' ' + d.prelude : ''} { ${d.style ? declText(d.style) : ''} }`;
    }
  };

  // ---- prototypes -------------------------------------------------------
  // Chrome's interface chain (the sweep minted these flat).
  const chain = (child, parent) => { const C = globalThis[child], P = globalThis[parent]; if (C && P && Object.getPrototypeOf(C.prototype) !== P.prototype) { Object.setPrototypeOf(C.prototype, P.prototype); Object.setPrototypeOf(C, P); } };
  for (const [c, p] of [['CSSGroupingRule', 'CSSRule'], ['CSSConditionRule', 'CSSGroupingRule'], ['CSSStyleRule', 'CSSGroupingRule'], ['CSSMediaRule', 'CSSConditionRule'], ['CSSSupportsRule', 'CSSConditionRule'], ['CSSContainerRule', 'CSSConditionRule'], ['CSSLayerBlockRule', 'CSSGroupingRule'], ['CSSScopeRule', 'CSSGroupingRule'], ['CSSStartingStyleRule', 'CSSGroupingRule'], ['CSSPageRule', 'CSSGroupingRule'], ['CSSKeyframesRule', 'CSSRule'], ['CSSKeyframeRule', 'CSSRule'], ['CSSFontFaceRule', 'CSSRule'], ['CSSImportRule', 'CSSRule'], ['CSSNamespaceRule', 'CSSRule'], ['CSSPropertyRule', 'CSSRule'], ['CSSCounterStyleRule', 'CSSRule'], ['CSSLayerStatementRule', 'CSSRule']]) chain(c, p);
  const acc = (C, name, get, set) => { if (C) Object.defineProperty(C.prototype, name, { get, set, enumerable: true, configurable: true }); };
  const TYPE = { style: 1, charset: 2, import: 3, media: 4, 'font-face': 5, page: 6, keyframes: 7, keyframe: 8, namespace: 10, 'counter-style': 11, supports: 12, 'font-feature-values': 14 };
  const R = globalThis.CSSRule;
  if (R) {
    for (const [k, v] of Object.entries({ STYLE_RULE: 1, CHARSET_RULE: 2, IMPORT_RULE: 3, MEDIA_RULE: 4, FONT_FACE_RULE: 5, PAGE_RULE: 6, KEYFRAMES_RULE: 7, KEYFRAME_RULE: 8, MARGIN_RULE: 9, NAMESPACE_RULE: 10, COUNTER_STYLE_RULE: 11, SUPPORTS_RULE: 12, FONT_FEATURE_VALUES_RULE: 14 })) {
      Object.defineProperty(R, k, { value: v, enumerable: true });
      Object.defineProperty(R.prototype, k, { value: v, enumerable: true });
    }
    acc(R, 'type', function () { return TYPE[rec(this).kind] || 0; });
    acc(R, 'cssText', function () { return ser(this); }, function () {});
    acc(R, 'parentRule', function () { const p = rec(this).parent; return p && D.has(p) ? p : null; });
    acc(R, 'parentStyleSheet', function () { return rec(this).sheet || null; });
  }
  const G = globalThis.CSSGroupingRule || globalThis.CSSRule;
  const grouping = [globalThis.CSSGroupingRule, globalThis.CSSStyleRule, globalThis.CSSKeyframesRule].filter(Boolean);
  for (const C of grouping) {
    acc(C, 'cssRules', function () { return rec(this).children; });
  }
  const mutate = (owner, fn) => { fn(rec(owner).children); const sh = rec(owner).sheet; if (sh && sh._cssomChanged) sh._cssomChanged(); };
  const insert = (owner, text, index) => {
    const kids = rec(owner).children;
    const idx = index === undefined ? 0 : index >>> 0;
    if (idx > kids.length) throw new DOMException("Failed to execute 'insertRule' on '" + owner.constructor.name + "': The index provided (" + idx + ") is larger than the maximum index (" + kids.length + ").", 'IndexSizeError');
    const items = scan(String(text));
    if (items.length !== 1) throw new DOMException("Failed to execute 'insertRule' on '" + owner.constructor.name + "': Failed to parse the rule '" + text + "'.", 'SyntaxError');
    const r = build(items[0], owner, rec(owner).sheet);
    if (!r) throw new DOMException("Failed to execute 'insertRule' on '" + owner.constructor.name + "': Failed to parse the rule '" + text + "'.", 'SyntaxError');
    mutate(owner, (k) => k.splice(idx, 0, r));
    return idx;
  };
  const remove = (owner, index) => {
    const kids = rec(owner).children;
    const i = index >>> 0;
    if (i >= kids.length) throw new DOMException("Failed to execute 'deleteRule' on '" + owner.constructor.name + "': The index provided (" + index + ") is larger than the maximum index (" + (kids.length - 1) + ").", 'IndexSizeError');
    mutate(owner, (k) => k.splice(i, 1));
  };
  for (const C of [globalThis.CSSGroupingRule, globalThis.CSSStyleRule].filter(Boolean)) {
    Object.defineProperty(C.prototype, 'insertRule', { value(t, i) { return insert(this, t, i); }, writable: true, configurable: true });
    Object.defineProperty(C.prototype, 'deleteRule', { value(i) { remove(this, i); }, writable: true, configurable: true });
  }
  acc(globalThis.CSSStyleRule, 'selectorText', function () { return rec(this).selector; }, function (v) { rec(this).selector = normSelector(String(v)); const sh = rec(this).sheet; if (sh && sh._cssomChanged) sh._cssomChanged(); });
  acc(globalThis.CSSStyleRule, 'style', function () { return rec(this).style; });
  for (const n of ['CSSFontFaceRule', 'CSSPageRule', 'CSSKeyframeRule', 'CSSPropertyRule', 'CSSCounterStyleRule']) acc(globalThis[n], 'style', function () { return rec(this).style; });
  acc(globalThis.CSSPageRule, 'selectorText', function () { return rec(this).prelude; });
  acc(globalThis.CSSKeyframeRule, 'keyText', function () { return rec(this).keyText; }, function (v) { rec(this).keyText = normKeyText(String(v)); });
  acc(globalThis.CSSKeyframesRule, 'name', function () { return rec(this).name; }, function (v) { rec(this).name = String(v); });
  if (globalThis.CSSKeyframesRule) {
    const P = globalThis.CSSKeyframesRule.prototype;
    Object.defineProperty(P, 'appendRule', { value(t) { const s = scan(String(t)); const x = s[0]; if (x && x.block) { const k = mk('CSSKeyframeRule', { kind: 'keyframe', keyText: normKeyText(x.head), parent: this, sheet: rec(this).sheet, style: decl(scan(x.body).filter((y) => !y.block).map((y) => y.head).join('; ')) }); mutate(this, (c) => c.push(k)); } }, writable: true, configurable: true });
    Object.defineProperty(P, 'deleteRule', { value(sel) { const key = normKeyText(String(sel)); const c = rec(this).children; const i = c.findIndex((k) => rec(k).keyText === key); if (i >= 0) mutate(this, (cc) => cc.splice(i, 1)); }, writable: true, configurable: true });
    Object.defineProperty(P, 'findRule', { value(sel) { const key = normKeyText(String(sel)); return rec(this).children.find((k) => rec(k).keyText === key) || null; }, writable: true, configurable: true });
  }
  const mediaList = (text) => {
    const items = splitTop(text, ',').map((x) => x.trim()).filter(Boolean);
    return { get length() { return items.length; }, item(i) { return items[i] ?? null; }, get mediaText() { return items.join(', '); }, set mediaText(v) { items.splice(0, items.length, ...splitTop(String(v), ',').map((x) => x.trim()).filter(Boolean)); }, appendMedium(m) { items.push(String(m)); }, deleteMedium(m) { const i = items.indexOf(String(m)); if (i >= 0) items.splice(i, 1); }, toString() { return items.join(', '); } };
  };
  acc(globalThis.CSSMediaRule, 'media', function () { const d = rec(this); return d.mediaObj || (d.mediaObj = mediaList(d.cond)); });
  for (const n of ['CSSMediaRule', 'CSSSupportsRule', 'CSSContainerRule', 'CSSScopeRule']) acc(globalThis[n], 'conditionText', function () { return rec(this).cond; });
  acc(globalThis.CSSContainerRule, 'containerName', function () { const m = /^([\w-]+)\s*(?:\(|$)/.exec(rec(this).cond); return m && !/^(not|and|or)$/i.test(m[1]) ? m[1] : ''; });
  acc(globalThis.CSSContainerRule, 'containerQuery', function () { return rec(this).cond.replace(/^[\w-]+\s+(?=\()/, ''); });
  acc(globalThis.CSSLayerBlockRule, 'name', function () { return rec(this).name; });
  acc(globalThis.CSSLayerStatementRule, 'nameList', function () { return rec(this).names.slice(); });
  acc(globalThis.CSSImportRule, 'href', function () { return rec(this).href; });
  acc(globalThis.CSSImportRule, 'media', function () { const d = rec(this); return d.mediaObj || (d.mediaObj = mediaList(d.rest.replace(/url\([^)]*\)|"[^"]*"|'[^']*'/, '').replace(/\blayer(\([^)]*\))?/, '').replace(/\bsupports\((?:[^()]|\([^()]*\))*\)/, '').trim())); });
  acc(globalThis.CSSImportRule, 'styleSheet', function () { return null; });
  acc(globalThis.CSSImportRule, 'layerName', function () { const m = /\blayer(?:\(([^)]*)\))?/.exec(rec(this).rest); return m ? (m[1] || '').trim() : null; });
  acc(globalThis.CSSNamespaceRule, 'namespaceURI', function () { return rec(this).uri; });
  acc(globalThis.CSSNamespaceRule, 'prefix', function () { return rec(this).prefix; });
  const propDesc = (name) => function () { const d = rec(this).style; return d ? d.getPropertyValue(name) : ''; };
  acc(globalThis.CSSPropertyRule, 'name', function () { return rec(this).prelude; });
  acc(globalThis.CSSPropertyRule, 'syntax', function () { return propDesc('syntax').call(this).replace(/^["']|["']$/g, ''); });
  acc(globalThis.CSSPropertyRule, 'inherits', function () { return propDesc('inherits').call(this).trim() === 'true'; });
  acc(globalThis.CSSPropertyRule, 'initialValue', function () { const v = propDesc('initial-value').call(this); return v === '' ? null : v; });
  acc(globalThis.CSSCounterStyleRule, 'name', function () { return rec(this).prelude; });

  // ---- the sheet --------------------------------------------------------
  const parseList = (text, sheet) => {
    const out = list([]);
    for (const it of scan(String(text))) { const r = build(it, null, sheet); if (r) out.push(r); }
    return out;
  };
  globalThis.__cssParseRules = (text, sheet) => parseList(text, sheet);
  const sheetText = (sheet) => sheet._rules.map((r) => r.cssText).join('\n');
  const CS = globalThis.CSSStyleSheet.prototype;
  const setRules = (sheet, rules) => {
    sheet._rules.length = 0;
    for (const r of rules) { D.get(r).parent = null; D.get(r).sheet = sheet; sheet._rules.push(r); }
    list(sheet._rules);
    sheet.cssRules = sheet._rules;
  };
  Object.defineProperty(CS, '_cssomChanged', {
    value() {
      if (typeof globalThis.__adoptedResync === 'function') globalThis.__adoptedResync();
      const el = this.ownerNode;
      if (el && el.nodeType === 1 && String(el.tagName).toLowerCase() === 'style' && el._nid != null) {
        try { _domRaw('ext_sheet_put', '\u0000style:' + el._nid, '\u0002' + sheetText(this)); } catch (e) {}
      }
    },
    writable: true, configurable: true,
  });
  const def = (n, f) => Object.defineProperty(CS, n, { value: f, writable: true, configurable: true });
  def('insertRule', function insertRule(text, index) {
    const idx = index === undefined ? 0 : index >>> 0;
    if (idx > this._rules.length) throw new DOMException("Failed to execute 'insertRule' on 'CSSStyleSheet': The index provided (" + idx + ") is larger than the maximum index (" + this._rules.length + ").", 'IndexSizeError');
    const items = scan(String(text));
    const r = items.length === 1 ? build(items[0], null, this) : null;
    if (!r) throw new DOMException("Failed to execute 'insertRule' on 'CSSStyleSheet': Failed to parse the rule '" + text + "'.", 'SyntaxError');
    // @import (and @layer statements) must lead the sheet; nothing else may precede an @import.
    const lead = (k) => k === 'import' || k === 'layerstmt';
    const kinds = this._rules.map((x) => rec(x).kind);
    const lastImport = kinds.lastIndexOf('import');
    if ((rec(r).kind === 'import' && kinds.slice(0, idx).some((k) => !lead(k))) || (rec(r).kind !== 'import' && lastImport >= idx)) {
      throw new DOMException("Failed to execute 'insertRule' on 'CSSStyleSheet': Failed to insert the rule.", 'HierarchyRequestError');
    }
    this._rules.splice(idx, 0, r);
    this.cssRules = this._rules;
    this._cssomChanged();
    return idx;
  });
  def('deleteRule', function deleteRule(index) {
    const i = index >>> 0;
    if (i >= this._rules.length) throw new DOMException("Failed to execute 'deleteRule' on 'CSSStyleSheet': The index provided (" + index + ") is larger than the maximum index (" + (this._rules.length - 1) + ").", 'IndexSizeError');
    this._rules.splice(i, 1);
    this._cssomChanged();
  });
  def('addRule', function addRule(selector, style, index) {
    this.insertRule(String(selector) + ' { ' + String(style) + ' }', index === undefined ? this._rules.length : index);
    return -1;
  });
  def('removeRule', function removeRule(index) { this.deleteRule(index === undefined ? 0 : index); });
  def('replaceSync', function replaceSync(text) {
    setRules(this, parseList(text, this).filter((r) => rec(r).kind !== 'import'));
    this._cssomChanged();
  });
  def('replace', function replace(text) {
    this.replaceSync(text);
    return Promise.resolve(this);
  });
  Object.defineProperty(CS, 'rules', { get() { return this._rules; }, configurable: true, enumerable: true });
  globalThis.__cssSetRules = setRules;
})();
