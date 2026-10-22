// getComputedStyle().width / .height as Chrome resolves them (split from
// bootstrap.js): the USED size in the box `box-sizing` selects — content box
// by default (border-box rect minus padding and border), untransformed, and
// the authored value / "auto" where no box applies (display:none, non-replaced
// inline). The old path echoed an inline declaration verbatim ("calc(...)",
// "50%") and otherwise returned the transformed border-box rect.
(() => {
  const REPLACED = new Set(['IMG', 'VIDEO', 'CANVAS', 'SVG', 'IFRAME', 'EMBED', 'OBJECT', 'INPUT', 'SELECT', 'TEXTAREA', 'BUTTON', 'METER', 'PROGRESS']);
  const px = (v) => { const n = parseFloat(v); return Number.isFinite(n) ? n : 0; };
  const fmt = (n) => `${Math.round(Math.max(0, n) * 1000) / 1000}px`;
  // `read(prop)` is the caller's computed-style lookup. Returns a string, or
  // null to let the caller's authored-value fallbacks run.
  globalThis.__usedSize = function (el, name, read) {
    const horizontal = name === 'width' || name === 'inline-size';
    const vertical = name === 'height' || name === 'block-size';
    if (!horizontal && !vertical) return null;
    if (!el || !el.isConnected) return null;
    const display = read('display');
    // No box: Chrome answers the specified value; the cascade value is not
    // visible from here, so the inline declaration or "auto".
    if (display === 'none' || display === 'contents') return (el.style && el.style.getPropertyValue(name)) || 'auto';
    if ((display === 'inline') && !REPLACED.has(String(el.tagName).toUpperCase())) return 'auto';
    // The pre-transform layout box (the rect carries transforms).
    const local = _ditingLocalBox(el);
    const r = local ? null : el.getBoundingClientRect();
    const w = local ? local.w : r.width, h = local ? local.h : r.height;
    const border = (side) => (read(`border-${side}-style`) === 'none' || read(`border-${side}-style`) === 'hidden') ? 0 : px(read(`border-${side}-width`));
    const edges = horizontal
      ? px(read('padding-left')) + px(read('padding-right')) + border('left') + border('right')
      : px(read('padding-top')) + px(read('padding-bottom')) + border('top') + border('bottom');
    const borderBox = read('box-sizing') === 'border-box';
    const full = horizontal ? w : h;
    return fmt(borderBox ? full : full - edges);
  };
})();
