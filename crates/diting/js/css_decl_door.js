// #148 CSSOM declaration door (split from bootstrap.js, ratchet): the
// pure length/percentage property table and the token-shape check
// CSSStyleDeclaration.setProperty / style-attribute parsing call at run time.
// #148: CSSOM setters validate before they store. Chrome's door is the full
// per-property grammar; this door is deliberately narrower — the pure
// length/percentage properties (positioning + box spacing, the family
// component libraries write from JS offset math) get a token-shape check
// that rejects the broken-math values: 'NaNpx', 'undefinedpx', 'Infinitypx',
// '[object Object]', bare unitless '10'. A rejected set is a no-op that
// keeps the previous value, like Chrome (headless ground truth: read-back
// after `style.left='NaNpx'` stays ''). Judgement-proof values — calc()/var()
// functions, family keywords, every property outside the table — store as
// before; the layout parser already drops what it rejects (#129 css_f32).
const _CSS_LENGTH_PROPS = new Set([
  "left", "right", "top", "bottom",
  "inset", "inset-inline", "inset-inline-start", "inset-inline-end",
  "inset-block", "inset-block-start", "inset-block-end",
  "width", "height", "min-width", "min-height", "max-width", "max-height",
  "inline-size", "block-size", "min-inline-size", "min-block-size",
  "max-inline-size", "max-block-size",
  "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
  "margin-inline", "margin-inline-start", "margin-inline-end",
  "margin-block", "margin-block-start", "margin-block-end",
  "padding", "padding-top", "padding-right", "padding-bottom", "padding-left",
  "padding-inline", "padding-inline-start", "padding-inline-end",
  "padding-block", "padding-block-start", "padding-block-end",
  "border-width", "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
  "gap", "row-gap", "column-gap", "grid-row-gap", "grid-column-gap",
  "text-indent", "letter-spacing", "word-spacing",
]);
const _CSS_LEN_KEYWORDS = /^(inherit|initial|unset|revert|auto|none|normal|thin|medium|thick)$/i;
const _CSS_LEN_TOKEN = new RegExp(
  "^[+-]?(?:\\d+\\.?\\d*|\\.\\d+)(?:e[+-]?\\d+)?(?:px|em|rem|%|vh|vw|vmin|vmax|svh|lvh|dvh|svw|lvw|dvw|ch|ex|q|mm|cm|in|pt|pc)$",
  "i"
);
const _CSS_NUM_TOKEN = /^[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?$/i;
const _CSS_FN_TOKEN = /^[a-zA-Z-]+\(.*\)$/;

// Split at paren/bracket depth 0 so function arguments keep their internal
// spaces ('calc(10px + 2em)' stays one token). Null = unbalanced → reject.
function _cssTopLevelTokens(v) {
  const out = [];
  let cur = "";
  let depth = 0;
  for (const ch of String(v)) {
    if (ch === "(" || ch === "[") depth++;
    else if (ch === ")" || ch === "]") { depth--; if (depth < 0) return null; }
    if (depth === 0 && /\s/.test(ch)) {
      if (cur) { out.push(cur); cur = ""; }
    } else cur += ch;
  }
  if (depth !== 0 || cur) out.push(cur);
  return depth === 0 ? out : null;
}

function _cssLengthValueOk(v) {
  const toks = _cssTopLevelTokens(v);
  if (!toks || !toks.length) return false;
  return toks.every((t) =>
    _CSS_LEN_KEYWORDS.test(t)
    || _CSS_LEN_TOKEN.test(t)
    || (_CSS_NUM_TOKEN.test(t) && parseFloat(t) === 0)
    || _CSS_FN_TOKEN.test(t)
  );
}

// The CSSOM-visible half of the declaration door: an invalid value never
// enters the style face, so read-backs keep the last good value.
function _cssDeclAccepted(dashedName, value) {
  if (!dashedName || dashedName.startsWith("--") || !_CSS_LENGTH_PROPS.has(dashedName)) return true;
  return _cssLengthValueOk(value);
}
