// IDL reflection for the list elements (split from bootstrap.js's
// _htmlInterface table, which has no signed-long mode): ol.start/reversed/
// type/compact, li.value/type, ul.type/compact. `long` getters parse the
// content attribute with the HTML integer rules (leading whitespace, optional
// sign, digits, stop at the first non-digit) and fall back to the default;
// setters write ToInt32 verbatim. Everything is call-time.
(() => {
  const def = (C, name, desc) => Object.defineProperty(C.prototype, name, { enumerable: true, configurable: true, ...desc });
  const long = (C, name, attr, dflt) => def(C, name, {
    get() {
      const m = /^[\t\n\f\r ]*([+-]?\d+)/.exec(this.getAttribute(attr) ?? '');
      if (m === null) return dflt;
      const n = parseInt(m[1], 10);
      return n >= -2147483648 && n <= 2147483647 ? n : dflt;
    },
    set(v) { this.setAttribute(attr, String(Number(v) | 0)); },
  });
  const str = (C, name, attr) => def(C, name, {
    get() { return this.getAttribute(attr) ?? ''; },
    set(v) { this.setAttribute(attr, String(v)); },
  });
  const bool = (C, name, attr) => def(C, name, {
    get() { return this.hasAttribute(attr); },
    set(v) { if (v) this.setAttribute(attr, ''); else this.removeAttribute(attr); },
  });
  long(HTMLOListElement, 'start', 'start', 1);
  bool(HTMLOListElement, 'reversed', 'reversed');
  str(HTMLOListElement, 'type', 'type');
  bool(HTMLOListElement, 'compact', 'compact');
  long(HTMLLIElement, 'value', 'value', 0);
  str(HTMLLIElement, 'type', 'type');
  str(HTMLUListElement, 'type', 'type');
  bool(HTMLUListElement, 'compact', 'compact');
})();

// CSSOM font-family serialization (CSS Fonts 4 §font-family + Chrome): a
// family that is one valid identifier reads back bare, anything else —
// multi-word names, names that need escaping, generic/CSS-wide keywords the
// author quoted — reads back as a double-quoted string, joined with ", ".
// Called from CSSStyleDeclaration.setProperty; var() and unbalanced quotes
// are stored verbatim, an invalid unquoted family returns null (the write
// is dropped, as in Chrome).
const _GENERIC_FAMILY = new Set(['serif', 'sans-serif', 'monospace', 'cursive', 'fantasy', 'system-ui', 'ui-serif', 'ui-sans-serif', 'ui-monospace', 'ui-rounded', 'emoji', 'math', 'fangsong', 'inherit', 'initial', 'unset', 'revert', 'revert-layer', 'default']);
function _cssFontFamilyCanon(v) {
  if (/var\(|env\(|attr\(/i.test(v)) return v;
  const parts = [];
  let cur = '', q = '';
  for (let i = 0; i < v.length; i++) {
    const c = v[i];
    if (q) {
      cur += c;
      if (c === '\\' && i + 1 < v.length) cur += v[++i];
      else if (c === q) q = '';
    } else if (c === '"' || c === "'") { q = c; cur += c; }
    else if (c === ',') { parts.push(cur); cur = ''; }
    else cur += c;
  }
  if (q) return v;
  parts.push(cur);
  const out = [];
  for (let p of parts) {
    p = p.trim();
    if (!p) return v;
    let name, quoted = false;
    if (p[0] === '"' || p[0] === "'") {
      if (p[p.length - 1] !== p[0] || p.length < 2) return v;
      quoted = true;
      name = p.slice(1, -1).replace(/\\(.)/g, '$1');
    } else {
      const words = p.split(/\s+/);
      // An unquoted family is a run of identifiers; anything else (a
      // leading digit, stray punctuation) invalidates the whole declaration.
      if (!words.every((w) => /^(?:-?[_a-zA-Z\u0080-\uFFFF]|--)[\w\u0080-\uFFFF-]*$/.test(w))) return null;
      name = words.join(' ');
    }
    const bare = /^(?:-?[_a-zA-Z\u0080-\uFFFF]|--)[\w\u0080-\uFFFF-]*$/.test(name);
    if (!quoted && _GENERIC_FAMILY.has(name.toLowerCase())) out.push(name);
    else if (bare && !(quoted && _GENERIC_FAMILY.has(name.toLowerCase()))) out.push(name);
    else out.push('"' + name.replace(/["\\]/g, '\\$&') + '"');
  }
  return out.join(', ');
}
