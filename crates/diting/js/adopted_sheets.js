// Constructable stylesheets (split from bootstrap.js): `new CSSStyleSheet()`
// sheets adopted through document/shadowRoot.adoptedStyleSheets join the
// cascade. The engine's style pool is built natively from the DOM, so the
// adopted sheets' text is parked in the native ext-sheet table under a
// reserved key (ext_sheet_put also drops the layout cache) and re-parked
// whenever an adopting list or an adopted sheet's rules change. Shadow-root
// adoption joins the same global pool the shadow <style> blocks already use.
(() => {
  const owners = new Set();
  const ownerSheets = (o) => o._adoptedStyleSheets || [];
  const sync = () => {
    let text = '';
    for (const o of owners) {
      for (const sheet of ownerSheets(o)) {
        if (!sheet || sheet.disabled) continue;
        for (const r of sheet._rules || []) text += r.cssText + '\n';
      }
    }
    try { _domRaw('ext_sheet_put', '\u0000adopted', text); } catch (e) {}
  };
  // Called by the Document/ShadowRoot adoptedStyleSheets setters (bootstrap).
  globalThis.__adoptedSet = (owner, sheets) => {
    for (const s of sheets || []) {
      if (!(s instanceof CSSStyleSheet)) {
        throw new TypeError("Failed to set the 'adoptedStyleSheets' property on '" + (owner.constructor && owner.constructor.name) + "': Failed to convert value to 'CSSStyleSheet'.");
      }
    }
    const list = Array.from(sheets || []);
    if (list.length) owners.add(owner); else owners.delete(owner);
    owner._adoptedStyleSheets = list;
    sync();
    return list;
  };
  // A sheet already adopted changes: push the new text. The check is cheap
  // when nothing is adopted.
  for (const m of ['insertRule', 'deleteRule', 'replace', 'replaceSync', 'addRule', 'removeRule']) {
    const orig = CSSStyleSheet.prototype[m];
    if (typeof orig !== 'function') continue;
    const wrapped = {
      [m](...args) {
        const r = orig.apply(this, args);
        if (owners.size) sync();
        return r;
      },
    }[m];
    Object.defineProperty(CSSStyleSheet.prototype, m, { value: wrapped, writable: true, configurable: true });
    _markNative(CSSStyleSheet.prototype[m]);
  }
})();
