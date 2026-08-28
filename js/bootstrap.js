"use strict";

// Eval shim backing Rust-side `evaluate`/`evaluate_for_cdp`. The input is
// evaluated with CDP Runtime.evaluate semantics first — a *script*, where
// statements are legal and the completion value of the last statement is the
// result (indirect eval = global scope, matching Chrome). Scripts written in
// function-body style with a top-level `return` are illegal as scripts; the
// SyntaxError is thrown before anything executes, so retrying them as a
// Function body is side-effect free. Runtime errors rethrow untouched — a
// retry would re-run side effects like document.write.
globalThis.__ditingEvalScript = function(src) {
  try { return (0, eval)(src); }
  catch (e) {
    if (e instanceof SyntaxError) { return (new Function(src))(); }
    throw e;
  }
};

globalThis.__diting_errors = [];

globalThis.addEventListener = globalThis.addEventListener || function(){};
globalThis.onunhandledrejection = function(e) { if (e?.preventDefault) e.preventDefault(); };

globalThis.onerror = function(msg, src, line, col, error) {
  globalThis.__diting_errors.push({msg: String(msg), src: String(src||""), line, error: String(error||"")});
};
globalThis.__windowListeners = {};
globalThis.addEventListener = function(type, fn) {
  if (!globalThis.__windowListeners[type]) globalThis.__windowListeners[type] = [];
  globalThis.__windowListeners[type].push(fn);
};
globalThis.removeEventListener = function(type, fn) {
  if (globalThis.__windowListeners[type]) {
    globalThis.__windowListeners[type] = globalThis.__windowListeners[type].filter(h => h !== fn);
  }
};
globalThis.dispatchEvent = function(event) {
  if (!event) return true;
  const handlers = globalThis.__windowListeners[event.type] || [];
  for (const h of handlers) { try { h.call(globalThis, event); } catch(e) { console.error(e); } }
  return !event.defaultPrevented;
};

// Receiver validation: real browsers throw TypeError("Illegal invocation")
// when a DOM method is called with a fake receiver (e.g.
// Object.create(HTMLSelectElement.prototype).setHTMLUnsafe(...) — a classic
// bot-detector probe). Our shims used to pass this._nid (undefined) down,
// where the Rust op's `parse().unwrap_or(0)` silently targeted node 0 — the
// DOCUMENT — and e.g. set_inner_html wiped the whole page. Commands not in
// _domStrA1 take a numeric node id as a1; reject anything else.
// Script-scoped capture of the deno op table. All runtime op calls go
// through this binding (not the `Deno` global) so __diting_init can DELETE
// globalThis.Deno / __bootstrap per page: a `Deno` property on window is the
// canonical marker of a deno-based runtime, and anti-bot collectors
// (WorkOS Radar) hash window property names. A top-level `const` creates a
// global *lexical* binding — it never appears on the global object at all.
const _OPS = Deno.core.ops;
// DOM mutation epoch (upstream's shape): every tree/attribute write bumps
// this at the one choke point all _domRaw commands pass through, so
// consumers of retained snapshots (getComputedStyle) can tell staleness
// without a native round-trip.
let _ditingMutationEpoch = 0;
const _DOM_MUTATION_COMMANDS = new Set([
  "append_child", "insert_before", "remove_child",
  "set_attribute", "remove_attribute",
  "set_text_content", "set_inner_html", "document_write_reset",
]);
const _domRaw = (cmd, a1, a2) => {
  if (_DOM_MUTATION_COMMANDS.has(cmd)) _ditingMutationEpoch++;
  return _OPS.op_dom(cmd, String(a1 ?? ""), String(a2 ?? ""));
};
const _domStrA1 = new Set([
  "create_element", "create_text_node", "create_comment_node",
  "create_processing_instruction", "create_doctype",
  "create_document_fragment",
  "query_selector", "query_selector_all", "get_element_by_id",
  "document_node_id", "document_title", "set_document_title", "document_referrer", "document_url", "document_base_url", "document_encoding",
  "document_element", "document_doctype",
  "document_write", "document_write_reset",
]);
const _domNumA2 = new Set(["append_child", "insert_before", "compare_order"]);
const _dom = (cmd, a1, a2) => {
  if (!_domStrA1.has(cmd) && (a1 === undefined || a1 === null || a1 === "" || isNaN(+a1))) {
    throw new TypeError("Illegal invocation");
  }
  if (_domNumA2.has(cmd) && (a2 === undefined || a2 === null || a2 === "" || isNaN(+a2))) {
    throw new TypeError("Illegal invocation");
  }
  return _domRaw(cmd, a1, a2);
};


const _nativeFns = new Set();
const _origToString = Function.prototype.toString;
// Method syntax gives the override the native function's shape: name
// "toString", length 0, non-constructible, and no own `prototype` property
// (upstream 846ed7d). A plain `function() {}` assignment leaks all four.
const _functionToString = {
  toString() {
    if (_nativeFns.has(this)) {
      return `function ${this.name || ''}() { [native code] }`;
    }
    return _origToString.call(this);
  },
}.toString;
Function.prototype.toString = _functionToString;
function _markNative(fn) { if (typeof fn === 'function') _nativeFns.add(fn); return fn; }
// Mark every method AND accessor on a shim prototype as native. Lie
// detectors (fingerprintjs-style) verify `Function.prototype.toString.call(fn)`
// reports [native code] for each API they probe — an unmarked shim flips
// their "API tampered" flag, and some detectors then run destructive probes
// in the main document as punishment.
function _markNativeProto(proto) {
  if (!proto) return;
  for (const p of Object.getOwnPropertyNames(proto)) {
    if (p === 'constructor') continue;
    const d = Object.getOwnPropertyDescriptor(proto, p);
    if (!d) continue;
    if (typeof d.value === 'function') _markNative(d.value);
    if (typeof d.get === 'function') _markNative(d.get);
    if (typeof d.set === 'function') _markNative(d.set);
  }
}
_nativeFns.add(_functionToString);

[Error, TypeError, ReferenceError, SyntaxError, RangeError, URIError, EvalError].forEach(E => {
  try {
    Object.defineProperty(E.prototype, 'name', {
      value: E.name, writable: true, enumerable: false, configurable: false,
    });
  } catch(e) {}
});

const _stackCache = new WeakMap();
const _origStackDesc = Object.getOwnPropertyDescriptor(Error.prototype, 'stack');
if (_origStackDesc && _origStackDesc.get) {
  Object.defineProperty(Error.prototype, 'stack', {
    configurable: false, enumerable: false,
    get: function() {
      if (!_stackCache.has(this)) _stackCache.set(this, _origStackDesc.get.call(this));
      return _stackCache.get(this);
    }
  });
}

let _fpSeed = 0;
function _fpRand(salt) {
  let h = (_fpSeed ^ (salt || 0)) | 0;
  h = Math.imul(h ^ (h >>> 16), 0x45d9f3b);
  h = Math.imul(h ^ (h >>> 13), 0x45d9f3b);
  return ((h ^ (h >>> 16)) >>> 0) / 0xFFFFFFFF;
}
function _fpNoise(x, y, channel) {
  return (_fpRand(x * 7919 + y * 6271 + channel * 8923) - 0.5) * 4;
}

var _fpCache = null;
// The persona must cohere with the UA the HTTP layer sends. A Windows
// Direct3D11 ANGLE renderer behind a macOS or Linux UA (what the old
// single-pool randomization produced) is a geographic/hardware
// impossibility that risk engines (Castle) score immediately.
function _fpPlatform() {
  const ua = globalThis.__diting_ua || '';
  if (ua.indexOf('Windows') !== -1) return 'win';
  if (ua.indexOf('Macintosh') !== -1 || ua.indexOf('Mac OS X') !== -1) return 'mac';
  if (ua.indexOf('Android') !== -1) return 'linux';
  return 'linux';
}
function _getFp() {
  // The cache is keyed by platform: during the V8 snapshot build no UA is
  // configured yet, so the first materialization defaults to the linux
  // persona. Once __diting_ua is set (per context, before __diting_init),
  // the cache rebuilds for the real platform instead of serving the frozen
  // snapshot-time one (which put Mesa GL strings behind a macOS UA).
  if (_fpCache && _fpCache.platform === _fpPlatform()) return _fpCache;
  const plat = _fpPlatform();
  // Per-platform GPU pools — vendor string and renderer must describe the
  // same machine the UA describes.
  const pools = {
    win: {
      gpu: [
        'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 SUPER Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (NVIDIA, NVIDIA GeForce RTX 2070 SUPER Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (AMD, AMD Radeon RX 580 Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (AMD, AMD Radeon RX 6700 XT Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (NVIDIA, NVIDIA GeForce RTX 4070 Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (NVIDIA, NVIDIA GeForce GTX 1080 Ti Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (Intel, Intel(R) UHD Graphics 770 Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (AMD, AMD Radeon RX 5700 XT Direct3D11 vs_5_0 ps_5_0, D3D11)',
        'ANGLE (NVIDIA, NVIDIA GeForce RTX 3080 Direct3D11 vs_5_0 ps_5_0, D3D11)',
      ],
      vendor: [
        'Google Inc. (NVIDIA)','Google Inc. (NVIDIA)','Google Inc. (NVIDIA)',
        'Google Inc. (Intel)','Google Inc. (Intel)',
        'Google Inc. (AMD)','Google Inc. (AMD)',
        'Google Inc. (NVIDIA)','Google Inc. (NVIDIA)',
        'Google Inc. (Intel)','Google Inc. (AMD)','Google Inc. (NVIDIA)',
      ],
      screens: [[1920,1080],[2560,1440],[1366,768],[1536,864],[1440,900],[1680,1050],[1280,720],[3840,2160]],
      dprs: [1,1,1,1.25,1,1,1,1],
    },
    mac: {
      gpu: [
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)',
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)',
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Pro, Unspecified Version)',
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)',
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M3 Pro, Unspecified Version)',
        'ANGLE (Apple, ANGLE Metal Renderer: Apple M4, Unspecified Version)',
        'ANGLE (Intel, Intel(R) Iris(TM) Plus Graphics 645, OpenGL 4.1)',
      ],
      vendor: [
        'Google Inc. (Apple)','Google Inc. (Apple)','Google Inc. (Apple)',
        'Google Inc. (Apple)','Google Inc. (Apple)','Google Inc. (Apple)',
        'Google Inc. (Intel)',
      ],
      screens: [[1512,982],[1728,1117],[1440,900],[1920,1080],[2560,1440],[1170,2532]],
      // Built-in retina panels run at 2x; the two external sizes run at 1x.
      dprs: [2,2,2,1,1,3],
    },
    linux: {
      gpu: [
        'ANGLE (Intel, Mesa Intel(R) UHD Graphics 630 (CML GT2), OpenGL 4.6 (Core Profile) Mesa 23.2.1)',
        'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.2.1)',
        'ANGLE (AMD, AMD Radeon RX 6700 XT (navi22, LLVM 15.0.7, DRM 3.54, LLVM 15.0.7), OpenGL 4.6 (Core Profile) Mesa 23.2.1)',
        'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 535.183.01)',
      ],
      vendor: [
        'Google Inc. (Intel)','Google Inc. (Intel)','Google Inc. (AMD)','Google Inc. (NVIDIA)',
      ],
      screens: [[1920,1080],[2560,1440],[1366,768],[3840,2160],[1680,1050]],
      dprs: [1,1,1,1,1],
    },
  };
  const pool = pools[plat];
  const idx = Math.floor(_fpRand(42) * pool.gpu.length);
  const sIdx = Math.floor(_fpRand(300) * pool.screens.length);
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  let cfp = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUg';
  for (let i = 0; i < 40; i++) cfp += chars[Math.floor(_fpRand(500 + i) * 64)];
  cfp += '==';
  _fpCache = {
    platform: plat,
    gpu: pool.gpu[idx], gpuVendor: pool.vendor[idx],
    screen: pool.screens[sIdx], dpr: pool.dprs[sIdx],
    audioBaseLatency: 0.002 + _fpRand(100) * 0.008,
    audioSampleRate: [44100, 48000][Math.floor(_fpRand(101) * 2)],
    compThreshold: -24 + (_fpRand(102) - 0.5) * 4,
    compKnee: 30 + (_fpRand(103) - 0.5) * 4,
    compRatio: 12 + (_fpRand(104) - 0.5) * 4,
    batteryLevel: 0.5 + _fpRand(200) * 0.5,
    batteryCharging: _fpRand(201) > 0.3,
    canvasFingerprint: cfp,
  };
  return _fpCache;
}
function _fp(key) { return _getFp()[key]; }
globalThis._eventRegistry = globalThis._eventRegistry || {};
globalThis._formValues = globalThis._formValues || {};
globalThis._formChecked = globalThis._formChecked || {};
globalThis._formIndeterminate = globalThis._formIndeterminate || {};
const _eventRegistry = globalThis._eventRegistry;
const _formValues = globalThis._formValues;
const _formChecked = globalThis._formChecked;
const _formIndeterminate = globalThis._formIndeterminate;
const _domParse = (cmd, a1, a2) => { try { return JSON.parse(_dom(cmd, a1, a2)); } catch { return null; } };

// HTML "ASCII whitespace": U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Class token splitting (classList, getElementsByClassName) uses exactly this set.
// JS \s is wider (U+000B, U+00A0, U+2028, etc.), so it must not be used here.
const _ASCII_WS = /[ \t\n\f\r]+/;
function _splitAsciiWhitespace(s) {
  // WebIDL DOMString coercion: null -> "null", undefined -> "undefined".
  return String(s).split(_ASCII_WS).filter(Boolean);
}
// Shared getElementsByClassName: split the argument into an ordered set of
// tokens on ASCII whitespace, then return descendants (in tree order) whose
// class attribute contains every token, as an HTMLCollection (so namedItem and
// named access work on the result). `root` must expose querySelectorAll.
function _getElementsByClassName(root, classNames) {
  const tokens = _splitAsciiWhitespace(classNames);
  if (tokens.length === 0) return HTMLCollection._from([]);
  // Fast path: a single CSS-identifier token goes straight to the native
  // selector engine (the common case). Only multi-token sets or exotic class
  // names (NBSP, leading digits, etc.) fall back to the O(n) JS scan below.
  if (tokens.length === 1 && /^[A-Za-z_-][\w-]*$/.test(tokens[0])) {
    return HTMLCollection._from(root.querySelectorAll("." + tokens[0]));
  }
  const all = root.querySelectorAll("*");
  const matched = [];
  for (let i = 0; i < all.length; i++) {
    const el = all[i];
    const elTokens = _splitAsciiWhitespace(el.getAttribute ? (el.getAttribute("class") || "") : "");
    let ok = true;
    for (let t = 0; t < tokens.length; t++) {
      if (elTokens.indexOf(tokens[t]) < 0) { ok = false; break; }
    }
    if (ok) matched.push(el);
  }
  return HTMLCollection._from(matched);
}
const _consoleFn = (level, args) => {
  try { _OPS.op_console_msg(level, args.map(a => {
    if (a === null) return "null";
    if (a === undefined) return "undefined";
    if (a instanceof Error) return a.stack || a.message || String(a);
    if (typeof a === "object") {
      try {
        const s = JSON.stringify(a);
        return s === "{}" && a.message ? a.message : s;
      } catch { return String(a); }
    }
    return String(a);
  }).join(" ")); } catch {}
  if (level === "error") {
    try {
      globalThis.__diting_errors = globalThis.__diting_errors || [];
      globalThis.__diting_errors.push({msg: args.map(a => {
        if (a && a.stack) return a.stack;
        if (a && a.message) return a.message;
        return String(a);
      }).join(" ")});
    } catch {}
  }
};

globalThis.console = {
  log: (...a) => _consoleFn("log", a), warn: (...a) => _consoleFn("warn", a),
  error: (...a) => _consoleFn("error", a), info: (...a) => _consoleFn("log", a),
  debug: () => {}, dir: () => {}, trace: () => {}, table: () => {}, group: () => {},
  groupEnd: () => {}, groupCollapsed: () => {}, time: () => {}, timeEnd: () => {},
  timeLog: () => {}, count: () => {}, countReset: () => {}, clear: () => {},
  assert: (c, ...a) => { if (!c) _consoleFn("error", ["Assertion failed:", ...a]); },
};

let _tid = 0;
const _clearedTimers = new Set();
const _intervals = new Set();

const _scheduleAfter = (delay, fn) => {
  const d = Math.max(0, Number(delay) || 0);
  // setTimeout(0) must be a MACROtask: it runs on a later event-loop turn,
  // after the current task's entire microtask queue has drained — the same
  // task boundary a real browser provides. Delivering it as a microtask
  // breaks every scheduler built on the distinction (React 19's Scheduler
  // defers work through MessageChannel/setTimeout specifically to land on
  // that boundary), so even delay-0 timers go through op_sleep: a resolved
  // async op is the cheapest true event-loop turn the runtime offers.
  _OPS.op_sleep(d).then(fn);
};

// Per HTML, a string timer handler is compiled and run as a classic script in
// global scope *at fire time*. Indirect eval ((0, eval)) runs in the true
// global scope, so top-level var/function declarations become globals (a
// `new Function(fn)` wrapper keeps them local); deferring to fire time also
// surfaces a SyntaxError when the timer elapses, matching a real browser,
// instead of swallowing it eagerly at scheduling (upstream #558). Function
// handlers pass through unchanged.
const _coerceTimerFn = (fn) => {
  if (typeof fn === "string") {
    const src = fn;
    return () => { (0, eval)(src); };
  }
  return typeof fn === "function" ? fn : null;
};

globalThis.setTimeout = (fn, delay = 0, ...args) => {
  const handler = _coerceTimerFn(fn);
  if (!handler) return ++_tid;
  const id = ++_tid;
  _scheduleAfter(delay, () => {
    if (_clearedTimers.has(id)) return;
    try { handler(...args); } catch(e) { console.error("Timer error:", e); }
  });
  return id;
};

globalThis.clearTimeout = (id) => { _clearedTimers.add(id); };

globalThis.setInterval = (fn, delay = 0, ...args) => {
  const handler = _coerceTimerFn(fn);
  if (!handler) return ++_tid;
  const id = ++_tid;
  _intervals.add(id);
  const tick = () => {
    if (!_intervals.has(id)) return;
    try { handler(...args); } catch(e) { console.error("Interval error:", e); }
    if (!_intervals.has(id)) return;
    _scheduleAfter(delay, tick);
  };
  _scheduleAfter(delay, tick);
  return id;
};

globalThis.clearInterval = (id) => { _intervals.delete(id); _clearedTimers.add(id); };
globalThis.requestAnimationFrame = (fn) => setTimeout(fn, 0);
globalThis.cancelAnimationFrame = globalThis.clearTimeout;
globalThis.queueMicrotask = globalThis.queueMicrotask || ((fn) => Promise.resolve().then(fn));

class MessageChannel {
  constructor() {
    this.port1 = { onmessage: null, postMessage: () => {}, close() {}, addEventListener() {}, removeEventListener() {} };
    this.port2 = { onmessage: null, postMessage: () => {}, close() {}, addEventListener() {}, removeEventListener() {} };
    // Message delivery is a macrotask, exactly like a real browser: React's
    // Scheduler posts work through a MessageChannel port precisely because
    // delivery lands on a fresh task after the microtask queue drains.
    // Microtask delivery interleaves scheduler work with unrelated promise
    // chains and deterministically wedges transitions (observed as "server
    // actions dispatch from some realms and never from others").
    this.port1.postMessage = (data) => {
      _scheduleAfter(0, () => { if (this.port2.onmessage) this.port2.onmessage({ data }); });
    };
    this.port2.postMessage = (data) => {
      _scheduleAfter(0, () => { if (this.port1.onmessage) this.port1.onmessage({ data }); });
    };
  }
}
globalThis.MessageChannel = MessageChannel;
globalThis.MessagePort = class MessagePort { constructor(){} postMessage(){} close(){} addEventListener(){} removeEventListener(){} };

const _cssCamelToKebab = (s) => String(s).replace(/[A-Z]/g, (m) => "-" + m.toLowerCase());
const _cssKebabToCamel = (s) => String(s).replace(/-([a-z])/g, (_, c) => c.toUpperCase());

// Standard CSS property names (camelCase). Real CSSStyleDeclaration exposes
// every property as an enumerable accessor, so feature-detection code
// (`'gap' in el.style`) and enumeration (`Object.keys(el.style)`) see the whole
// set, not just the ones that happen to be assigned.
const _CSS_PROPERTY_NAMES = [
  "accentColor","alignContent","alignItems","alignSelf","all","animation","animationDelay",
  "animationDirection","animationDuration","animationFillMode","animationIterationCount",
  "animationName","animationPlayState","animationTimingFunction","appearance","aspectRatio",
  "backdropFilter","backfaceVisibility","background","backgroundAttachment","backgroundBlendMode",
  "backgroundClip","backgroundColor","backgroundImage","backgroundOrigin","backgroundPosition",
  "backgroundPositionX","backgroundPositionY","backgroundRepeat","backgroundSize","blockSize",
  "border","borderBlock","borderBlockColor","borderBlockEnd","borderBlockEndColor","borderBlockEndStyle",
  "borderBlockEndWidth","borderBlockStart","borderBlockStartColor","borderBlockStartStyle",
  "borderBlockStartWidth","borderBlockStyle","borderBlockWidth","borderBottom","borderBottomColor",
  "borderBottomLeftRadius","borderBottomRightRadius","borderBottomStyle","borderBottomWidth",
  "borderCollapse","borderColor","borderImage","borderImageOutset","borderImageRepeat",
  "borderImageSlice","borderImageSource","borderImageWidth","borderInline","borderInlineColor",
  "borderInlineEnd","borderInlineEndColor","borderInlineEndStyle","borderInlineEndWidth",
  "borderInlineStart","borderInlineStartColor","borderInlineStartStyle","borderInlineStartWidth",
  "borderInlineStyle","borderInlineWidth","borderLeft","borderLeftColor","borderLeftStyle",
  "borderLeftWidth","borderRadius","borderRight","borderRightColor","borderRightStyle",
  "borderRightWidth","borderSpacing","borderStyle","borderTop","borderTopColor","borderTopLeftRadius",
  "borderTopRightRadius","borderTopStyle","borderTopWidth","borderWidth","bottom","boxShadow",
  "boxSizing","breakAfter","breakBefore","breakInside","captionSide","caretColor","clear","clip",
  "clipPath","color","colorScheme","columnCount","columnFill","columnGap","columnRule","columnRuleColor",
  "columnRuleStyle","columnRuleWidth","columnSpan","columnWidth","columns","contain","container",
  "containerName","containerType","content","counterIncrement","counterReset","counterSet","cssFloat",
  "cursor","direction","display","emptyCells","filter","flex","flexBasis","flexDirection","flexFlow",
  "flexGrow","flexShrink","flexWrap","float","font","fontFamily","fontFeatureSettings","fontKerning",
  "fontOpticalSizing","fontSize","fontSizeAdjust","fontStretch","fontStyle","fontVariant",
  "fontVariantCaps","fontVariantLigatures","fontVariantNumeric","fontWeight","gap","grid","gridArea",
  "gridAutoColumns","gridAutoFlow","gridAutoRows","gridColumn","gridColumnEnd","gridColumnGap",
  "gridColumnStart","gridGap","gridRow","gridRowEnd","gridRowGap","gridRowStart","gridTemplate",
  "gridTemplateAreas","gridTemplateColumns","gridTemplateRows","height","hyphens","imageRendering",
  "inlineSize","inset","insetBlock","insetBlockEnd","insetBlockStart","insetInline","insetInlineEnd",
  "insetInlineStart","isolation","justifyContent","justifyItems","justifySelf","left","letterSpacing",
  "lineBreak","lineHeight","listStyle","listStyleImage","listStylePosition","listStyleType","margin",
  "marginBlock","marginBlockEnd","marginBlockStart","marginBottom","marginInline","marginInlineEnd",
  "marginInlineStart","marginLeft","marginRight","marginTop","mask","maxBlockSize","maxHeight",
  "maxInlineSize","maxWidth","minBlockSize","minHeight","minInlineSize","minWidth","mixBlendMode",
  "objectFit","objectPosition","offset","opacity","order","outline","outlineColor","outlineOffset",
  "outlineStyle","outlineWidth","overflow","overflowAnchor","overflowWrap","overflowX","overflowY",
  "overscrollBehavior","overscrollBehaviorBlock","overscrollBehaviorInline","overscrollBehaviorX",
  "overscrollBehaviorY","padding","paddingBlock","paddingBlockEnd","paddingBlockStart","paddingBottom",
  "paddingInline","paddingInlineEnd","paddingInlineStart","paddingLeft","paddingRight","paddingTop",
  "pageBreakAfter","pageBreakBefore","pageBreakInside","perspective","perspectiveOrigin","placeContent",
  "placeItems","placeSelf","pointerEvents","position","quotes","resize","right","rotate","rowGap",
  "scale","scrollBehavior","scrollMargin","scrollPadding","scrollSnapAlign","scrollSnapStop",
  "scrollSnapType","tabSize","tableLayout","textAlign","textAlignLast","textCombineUpright",
  "textDecoration","textDecorationColor","textDecorationLine","textDecorationSkipInk",
  "textDecorationStyle","textDecorationThickness","textEmphasis","textIndent","textJustify",
  "textOrientation","textOverflow","textRendering","textShadow","textTransform","textUnderlineOffset",
  "textUnderlinePosition","top","touchAction","transform","transformBox","transformOrigin",
  "transformStyle","transition","transitionDelay","transitionDuration","transitionProperty",
  "transitionTimingFunction","translate","unicodeBidi","userSelect","verticalAlign","visibility",
  "whiteSpace","width","willChange","wordBreak","wordSpacing","wordWrap","writingMode","zIndex","zoom",
];
const _CSS_PROP_SET = new Set(_CSS_PROPERTY_NAMES);

// Parse a `style` attribute string (`"color: red; margin: 5px"`) into the given
// dashed-key store, replacing its contents in place.
function _parseCssInto(props, text) {
  for (const k in props) delete props[k];
  if (text) String(text).split(";").forEach((p) => {
    const i = p.indexOf(":");
    if (i > 0) { const k = p.slice(0, i).trim(); const v = p.slice(i + 1).trim(); if (k && v) props[_cssCamelToKebab(k)] = v; }
  });
}
function _serializeCss(props) {
  const e = Object.entries(props);
  return e.length ? e.map(([k, v]) => `${k}: ${v}`).join("; ") + ";" : "";
}

class CSSStyleDeclaration {
  constructor(owner) {
    // Non-enumerable so they never leak through the proxy's own-key traps.
    Object.defineProperty(this, "_props", { value: {}, writable: true, enumerable: false, configurable: true });
    // The owner Element, if any. A live declaration reflects that element's
    // `style` content attribute in both directions; an owner-less declaration
    // (getComputedStyle fallback, stylesheet rules) is purely in-memory.
    Object.defineProperty(this, "_owner", { value: owner || null, writable: true, enumerable: false, configurable: true });
    // Last `style` attribute string we parsed/wrote, so a read can skip the
    // reparse when the attribute has not changed underneath us. Held in a
    // one-field object so the sync helpers can mutate it without a bare
    // `this._x = …` assignment (which the style proxy would reroute into
    // setProperty).
    Object.defineProperty(this, "_sync", { value: { last: null }, writable: true, enumerable: false, configurable: true });
  }
  // Pull the owner's `style` attribute into `_props` if it changed since our
  // last read/write. Keeps parsed HTML and setAttribute('style', …) visible via
  // el.style.*. No-op when owner-less.
  _pull() {
    const o = this._owner;
    if (!o) return;
    const attr = o.getAttribute("style");
    if (attr === this._sync.last) return;
    _parseCssInto(this._props, attr);
    this._sync.last = attr;
  }
  // Serialize `_props` back onto the owner's `style` attribute after a mutation,
  // so el.style.x = … and cssText reflect into getAttribute('style') and
  // serialization. No-op when owner-less.
  _push() {
    const o = this._owner;
    if (!o) return;
    const text = _serializeCss(this._props);
    this._sync.last = text;
    if (text) o.setAttribute("style", text);
    else o.removeAttribute("style");
  }
  // Storage is keyed by the dashed CSS name, matching CSSOM. The proxy maps the
  // camelCase IDL access (el.style.fontSize) onto the dashed key (font-size), so
  // getPropertyValue('font-size') and el.style.fontSize stay in sync.
  setProperty(name, value) {
    this._pull();
    const k = _cssCamelToKebab(String(name));
    if (value === "" || value == null) delete this._props[k];
    else this._props[k] = String(value);
    this._push();
  }
  removeProperty(name) { this._pull(); const k = _cssCamelToKebab(String(name)); const old = this._props[k]; delete this._props[k]; this._push(); return old || ""; }
  getPropertyValue(name) { this._pull(); return this._props[_cssCamelToKebab(String(name))] || ""; }
  getPropertyPriority() { return ""; }
  get cssText() { this._pull(); return _serializeCss(this._props); }
  set cssText(v) {
    _parseCssInto(this._props, v);
    this._push();
  }
  get length() { this._pull(); return Object.keys(this._props).length; }
  item(i) { this._pull(); return Object.keys(this._props)[i] || ""; }
}
// Legacy Blink interface: real Chrome exposes `CSS2Properties` with style
// property accessors living on its prototype, chained BELOW
// CSSStyleDeclaration.prototype. Bot detectors (Castle) probe
// `CSSStyleDeclaration.prototype` for it — a missing global is flagged as a
// tampered API, which flips their iframe fallback and runs destructive DOM
// probes in the main document.
globalThis.CSS2Properties = class CSS2Properties {};
Object.setPrototypeOf(CSSStyleDeclaration.prototype, CSS2Properties.prototype);
for (const _m of ["setProperty", "getPropertyValue", "getPropertyPriority", "removeProperty", "item"]) {
  if (typeof CSSStyleDeclaration.prototype[_m] === "function") {
    CSS2Properties.prototype[_m] = CSSStyleDeclaration.prototype[_m];
  }
}
_markNative(CSS2Properties);
_markNativeProto(CSSStyleDeclaration.prototype);
_markNativeProto(CSS2Properties.prototype);

// DOMStringMap backs Element.dataset. Chrome throws on `new DOMStringMap()`;
// the construction key keeps instances engine-only (upstream ec05ed0).
const _domStringMapKey = {};
class DOMStringMap {
  constructor(key) {
    if (key !== _domStringMapKey) {
      throw new TypeError("Failed to construct 'DOMStringMap': Illegal constructor");
    }
  }
  get [Symbol.toStringTag]() { return "DOMStringMap"; }
}

const _styleProxy = (decl) => new Proxy(decl, {
  get(t, p) {
    if (typeof p === "symbol" || p in t) return t[p];
    if (/^\d+$/.test(p)) return t.item(+p);
    return t.getPropertyValue(p);
  },
  set(t, p, v) {
    if (typeof p === "symbol") { t[p] = v; return true; }
    if (p === "cssText") { t.cssText = v; return true; }
    if (/^\d+$/.test(p) || p in Object.getPrototypeOf(t)) return true;
    t.setProperty(p, v);
    return true;
  },
  has(t, p) {
    if (typeof p !== "string") return Reflect.has(t, p);
    if (p in Object.getPrototypeOf(t)) return true;
    t._pull();
    if (_cssCamelToKebab(p) in t._props) return true;
    if (_CSS_PROP_SET.has(p) || _CSS_PROP_SET.has(_cssKebabToCamel(p))) return true;
    return /^\d+$/.test(p) && +p < t.length;
  },
  ownKeys(t) {
    t._pull();
    const keys = [];
    const n = t.length;
    for (let i = 0; i < n; i++) keys.push(String(i));
    const names = new Set(_CSS_PROPERTY_NAMES);
    for (const k of Object.keys(t._props)) names.add(_cssKebabToCamel(k));
    for (const name of names) keys.push(name);
    return keys;
  },
  getOwnPropertyDescriptor(t, p) {
    if (typeof p !== "string") return Reflect.getOwnPropertyDescriptor(t, p);
    t._pull();
    if (/^\d+$/.test(p) && +p < t.length) return { value: t.item(+p), writable: false, enumerable: true, configurable: true };
    if (_cssCamelToKebab(p) in t._props || _CSS_PROP_SET.has(p) || _CSS_PROP_SET.has(_cssKebabToCamel(p))) {
      return { value: t.getPropertyValue(p), writable: true, enumerable: true, configurable: true };
    }
    return undefined;
  },
});

// Shallow-clone one node as a real DOM node: text/comment copy their data,
// fragments come back empty, elements are recreated in their namespace with all
// attributes copied. Used by cloneNode's non-element deep path.
function _shallowCloneNode(node) {
  const nt = node.nodeType;
  if (nt === 3) return document.createTextNode(node.data != null ? node.data : (node.textContent || ""));
  if (nt === 8) return document.createComment(node.data != null ? node.data : (node.nodeValue || ""));
  if (nt === 11) return document.createDocumentFragment();
  if (nt !== 1) return null;
  const ns = node.namespaceURI;
  const el = (ns && ns !== "http://www.w3.org/1999/xhtml")
    ? document.createElementNS(ns, node.nodeName)
    : document.createElement(node.localName || node.nodeName.toLowerCase());
  const names = node.getAttributeNames ? node.getAttributeNames() : [];
  for (const name of names) {
    const v = node.getAttribute(name);
    if (v !== null) el.setAttribute(name, v);
  }
  // CSS declarations currently live on the JS wrapper independently of the
  // DOM attribute. Copy that state as well so styles assigned through
  // `node.style` survive cloning even before attribute reflection runs.
  if (node.style && node.style.cssText) el.style.cssText = node.style.cssText;
  return el;
}

// Prepare a single script element for execution, unless it has already
// started. The native flag (op_script_try_start) is authoritative and survives
// moves and cloneNode(), so a script inserted a second time runs no more than
// once (upstream 41a8e1c).
// Decode a data: URL whose payload is JavaScript (upstream 0c4740a + f841205
// final form). op_fetch_url's HTTP client cannot fetch the data: scheme, so
// dynamic <script src="data:text/javascript,..."> never ran. Chromium accepts
// any MIME type here (the fetch layer is bypassed entirely), rejects stray
// padding in base64, and handles %-escapes + non-ASCII by re-encoding through
// UTF-8 — matching that shape byte-for-byte avoids a fidelity gap pages can
// probe. _hexv is defined later in this file; resolution happens at call time.
function _decodeDataScriptUrl(url) {
  const comma = url.indexOf(',');
  if (!url.startsWith('data:') || comma < 5) {
    throw new TypeError('Invalid dynamic script data URL');
  }

  const meta = url.slice(5, comma);
  const fragment = url.indexOf('#', comma + 1);
  const payload = url.slice(comma + 1, fragment < 0 ? url.length : fragment);
  if (meta.split(';').some(part => part.toLowerCase() === 'base64')) {
    let encoded = payload.replace(/[\r\n\t\f ]/g, '');
    const remainder = encoded.length % 4;
    if (remainder === 1 || !/^[A-Za-z0-9+/]*={0,2}$/.test(encoded) || /=/.test(encoded.slice(0, -2))) {
      throw new TypeError('Invalid dynamic script data URL base64');
    }
    if (remainder > 0) encoded += '='.repeat(4 - remainder);
    if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(encoded)) {
      throw new TypeError('Invalid dynamic script data URL base64');
    }
    return new TextDecoder().decode(_base64ToUint8Array(encoded));
  }

  const bytes = [];
  for (let i = 0; i < payload.length; i++) {
    const code = payload.charCodeAt(i);
    if (code === 0x25 && i + 2 < payload.length) {
      const hi = _hexv(payload.charCodeAt(i + 1));
      const lo = _hexv(payload.charCodeAt(i + 2));
      if (hi >= 0 && lo >= 0) {
        bytes.push(hi * 16 + lo);
        i += 2;
        continue;
      }
    }
    if (code < 0x80) {
      bytes.push(code);
    } else {
      const character = String.fromCodePoint(payload.codePointAt(i));
      if (character.length === 2) i++;
      const encoded = new TextEncoder().encode(character);
      for (let j = 0; j < encoded.length; j++) bytes.push(encoded[j]);
    }
  }
  return new TextDecoder().decode(new Uint8Array(bytes));
}

// A valid form submitter: <button> that is not reset/button, or
// <input type=submit|image>. Shared by requestSubmit's argument validation
// and the internal click path, so a click can never hand requestSubmit a
// submitter it would reject (upstream 7e2cabf/ccfa5fb).
function _isSubmitButton(el) {
  if (!el || typeof el.localName !== "string") return false;
  const type = ((el.getAttribute && el.getAttribute("type")) || "").toLowerCase();
  if (el.localName === "button") return type !== "reset" && type !== "button";
  if (el.localName === "input") return type === "submit" || type === "image";
  return false;
}

// Labelable elements per the HTML spec: button, input (except type=hidden),
// meter, output, progress, select, textarea. A <label> activates its labeled
// control: the element referenced by `for`, else the first labelable descendant.
const _LABELABLE = 'button,input:not([type=hidden]),meter,output,progress,select,textarea';
function _labeledControl(label) {
  if (!label || label.tagName !== 'LABEL') return null;
  // A present `for` attribute means association by id only; an empty value
  // associates nothing (no fallback to a descendant).
  const forId = label.getAttribute ? label.getAttribute('for') : null;
  if (forId !== null && forId !== undefined) {
    if (forId === '') return null;
    const doc = label.ownerDocument || globalThis.document;
    const el = doc && doc.getElementById ? doc.getElementById(forId) : null;
    if (!el) return null;
    return el.matches && el.matches(_LABELABLE) ? el : null;
  }
  return label.querySelector ? label.querySelector(_LABELABLE) : null;
}

// The first <legend> child of a fieldset — the only legend whose descendants
// a disabled fieldset does NOT disable (HTML spec: "the first legend child").
function _firstLegend(fieldset) {
  const kids = fieldset.children;
  for (let i = 0; i < kids.length; i++) if (kids[i].tagName === 'LEGEND') return kids[i];
  return null;
}

// A control inside <fieldset disabled> is disabled unless the path from it to
// the fieldset crosses that fieldset's first <legend>. Walk ancestors; at each
// disabled fieldset on the path, the child we came through must be its first
// legend or the control is fieldset-disabled.
function _fieldsetDisabled(el) {
  let node = el;
  while (node) {
    const p = node.parentNode;
    if (!p || !p.tagName) break;
    if (p.tagName === 'FIELDSET' && p.hasAttribute && p.hasAttribute('disabled')) {
      if (!(node.tagName === 'LEGEND' && node === _firstLegend(p))) return true;
    }
    node = p;
  }
  return false;
}

// Actually-disabled per spec: a form control with its own disabled attribute,
// or any form control disabled by an ancestor fieldset. Disabled controls have
// no activation behaviour and dispatch no click event at all.
function _isFormControlDisabled(el) {
  const t = el.tagName;
  if (t !== 'INPUT' && t !== 'BUTTON' && t !== 'SELECT' && t !== 'TEXTAREA') return false;
  if ((el.hasAttribute && el.hasAttribute('disabled')) || el.disabled) return true;
  return _fieldsetDisabled(el);
}

function __prepareInsertedScript(script) {
  if (!_OPS.op_script_try_start(script._nid)) return;
  const scriptType = (script.getAttribute('type') || '').trim().toLowerCase();
  const isModule = scriptType === 'module';
  const isImportMap = scriptType === 'importmap';
  if (isImportMap) {
    // Upstream 34373c3: a dynamically inserted import map registers at its
    // insertion point, using the live document base URL. External import
    // maps are not supported (matching this engine's module pipeline).
    const src = script.getAttribute('src');
    let error = '';
    if (src) {
      error = 'External import maps are not supported';
    } else {
      const base = script.baseURI
        || globalThis.location?.href
        || 'about:blank';
      try {
        error = _OPS.op_add_import_map(script.textContent || '', base) || '';
      } catch (e) {
        error = e && e.message ? e.message : String(e);
      }
    }
    if (error) {
      console.error('Import map error:', error);
      queueMicrotask(() => {
        try { script.dispatchEvent(new Event('error')); } catch (_) {}
      });
    }
    return;
  }
  if (scriptType && !isModule && scriptType !== 'text/javascript' && scriptType !== 'application/javascript') {
    return;
  }
  const src = script.getAttribute('src');
  const code = src ? "" : script.textContent;
  if (!src && !code) return;
  const prevNid = globalThis.__currentScriptNid;
  if (src) {
    const fullUrl = src.startsWith('http') || src.startsWith('data:')
      ? src
      : new URL(src, globalThis.location?.href || 'http://localhost/').href;
    const pageOrigin = (function() { try { return new URL(globalThis.location?.href || "about:blank").origin; } catch(e) { return ""; } })();
    (async () => {
      try {
        if (isModule) {
          await import(fullUrl);
        } else {
          // The HTML script-fetch algorithm treats an unsuccessful HTTP
          // response as a network error — its body must never become script
          // source (404/500 diagnostic HTML executing as code is both unlike
          // browsers and dangerous). data: URLs bypass the HTTP client
          // entirely and decode in JS (f61493f + 0c4740a/f841205).
          let body;
          if (fullUrl.startsWith('data:')) {
            body = _decodeDataScriptUrl(fullUrl);
          } else {
            // Bracket the fetch so the settle loop keeps pumping past its
            // fast-path deadline while this script is still in flight.
            _OPS.op_dyn_script_fetch_begin();
            try {
              const raw = await _OPS.op_fetch_url(fullUrl, "GET", "{}", "", pageOrigin, "no-cors", "same-origin");
              const parsed = JSON.parse(raw);
              if (!(parsed.status >= 200 && parsed.status <= 299)) {
                throw new Error('HTTP ' + (parsed.status || 0));
              }
              body = parsed.body;
            } finally {
              _OPS.op_dyn_script_fetch_end();
            }
          }
          if (body) {
            globalThis.__currentScriptNid = script._nid;
            try { (0, eval)(body); }
            catch(e) { console.error('Dynamic script error (' + fullUrl + '):', e.message); }
            finally { globalThis.__currentScriptNid = prevNid || 0; }
          }
        }
        if (typeof script.onload === 'function') try { script.onload(new Event('load')); } catch(e) {}
          try { script.dispatchEvent(new Event('load')); } catch(e) {}
      } catch(e) {
        console.error('Dynamic script fetch error:', e.message);
        // Mirror the load path: both the onerror property and registered
        // listeners fire (a listener-only consumer must still see failures).
        const ev = new Event('error');
        if (typeof script.onerror === 'function') try { script.onerror(ev); } catch(ex) {}
          try { script.dispatchEvent(ev); } catch(ex) {}
      }
    })();
  } else {
    if (code) {
      if (isModule) {
        const dataUrl = 'data:text/javascript;base64,' + btoa(unescape(encodeURIComponent(code)));
        (async () => {
          try { await import(dataUrl); }
          catch(e) { console.error('Dynamic inline module error:', e.message); }
        })();
      } else {
        globalThis.__currentScriptNid = script._nid;
        try { (0, eval)(code); }
        catch(e) { console.error('Dynamic inline script error:', e.message); }
        finally { globalThis.__currentScriptNid = prevNid || 0; }
      }
    }
  }
}

// HTML's script preparation algorithm leaves a disconnected script unstarted.
// When an ancestor is later connected, insertion steps visit every script in
// that subtree in tree order.
function __prepareInsertedSubtree(root) {
  if (!root || !root.isConnected) return;
  const scripts = [];
  const seen = new Set();
  if (root.nodeType === 1 && root.tagName === 'SCRIPT') {
    scripts.push(root);
    seen.add(root._nid);
  }
  const ids = _domParse("query_selector_all_scoped", root._nid, "script") || [];
  for (const nid of ids) {
    const script = _wrapEl(+nid);
    if (script && !seen.has(script._nid)) {
      scripts.push(script);
      seen.add(script._nid);
    }
  }
  for (const script of scripts) __prepareInsertedScript(script);
}

class Node {
  static ELEMENT_NODE = 1;
  static ATTRIBUTE_NODE = 2;
  static TEXT_NODE = 3;
  static CDATA_SECTION_NODE = 4;
  static ENTITY_REFERENCE_NODE = 5;
  static ENTITY_NODE = 6;
  static PROCESSING_INSTRUCTION_NODE = 7;
  static COMMENT_NODE = 8;
  static DOCUMENT_NODE = 9;
  static DOCUMENT_TYPE_NODE = 10;
  static DOCUMENT_FRAGMENT_NODE = 11;
  static NOTATION_NODE = 12;
  static DOCUMENT_POSITION_DISCONNECTED = 1;
  static DOCUMENT_POSITION_PRECEDING = 2;
  static DOCUMENT_POSITION_FOLLOWING = 4;
  static DOCUMENT_POSITION_CONTAINS = 8;
  static DOCUMENT_POSITION_CONTAINED_BY = 16;
  static DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC = 32;

  constructor(nid) { this._nid = nid; }
  get nodeType() { return +_dom("node_type", this._nid); }
  get nodeName() { return _domParse("node_name", this._nid) || ""; }
  get ownerDocument() { return globalThis.document; }
  // https://dom.spec.whatwg.org/#dom-node-baseuri
  get baseURI() {
    try {
      const doc = globalThis.document;
      const docUrl = (doc && doc.URL) || "";
      const baseEl = (doc && doc.querySelector) ? doc.querySelector("base[href]") : null;
      if (baseEl) {
        const href = baseEl.getAttribute("href");
        if (href) {
          return docUrl ? new URL(href, docUrl).href : href;
        }
      }
      return docUrl;
    } catch (e) {
      return "";
    }
  }
  get textContent() { return _domParse("text_content", this._nid) ?? ""; }
  set textContent(v) {
    const oldChildren = _domParse("child_nodes", this._nid) || [];
    for (const c of oldChildren) _dom("remove_child", c);
    let added = [];
    if (v != null && v !== "") {
      const tn = +_dom("create_text_node", String(v));
      _dom("append_child", this._nid, tn);
      added = [tn];
    }
    // Real MutationObserver fires childList for the children swap.
    // Without this React 18+ hydration mismatch detection and many polling
    // libs (intersection-driven lazy load, content sync) silently stall.
    if (globalThis.__mutationObservers?.length) {
      globalThis.__notifyMutation('childList', this._nid, added, oldChildren);
    }
  }
  get nodeValue() {
    const t = this.nodeType;
    if (t === 3 || t === 8) return _domParse("text_content", this._nid) ?? "";
    return null;
  }
  set nodeValue(v) {
    const t = this.nodeType;
    if (t === 3 || t === 8) _dom("set_text_content", this._nid, String(v ?? ""));
  }
  get parentNode() { return _wrap(+_dom("parent_node", this._nid)); }
  get parentElement() { const p = this.parentNode; return p && p.nodeType === 1 ? p : null; }
  get childNodes() {
    const ids = _domParse("child_nodes", this._nid) || [];
    return _nodeList(ids.map(_wrap).filter(Boolean));
  }
  get firstChild() { return _wrap(+_dom("first_child", this._nid)); }
  get lastChild() { return _wrap(+_dom("last_child", this._nid)); }
  get nextSibling() { return _wrap(+_dom("next_sibling", this._nid)); }
  get previousSibling() { return _wrap(+_dom("prev_sibling", this._nid)); }
  appendChild(c) {
    if (!c) return c;
    // Per DOM spec, inserting a DocumentFragment inserts its CHILDREN — the
    // fragment itself never enters the tree (a `#document-fragment` visible
    // in body.childNodes is an instant bot tell).
    if (c.nodeType === 11 && c.nodeName === '#document-fragment') {
      const kids = Array.from(c.childNodes);
      for (const k of kids) this.appendChild(k);
      if (globalThis.__mutationObservers?.length) {
        globalThis.__notifyMutation('childList', this._nid, kids.map(k => k._nid), []);
      }
      if (_nodeInDocument(this)) for (const k of kids) _registerIframesIn(k);
      return c;
    }
    _dom("append_child", this._nid, c._nid);
    if (globalThis.__mutationObservers?.length) globalThis.__notifyMutation('childList', this._nid, [c._nid], []);
    // Insertion into the document instantiates browsing contexts for any
    // iframes in the inserted subtree (window[N] / window.length).
    if (_nodeInDocument(this)) _registerIframesIn(c);
    __prepareInsertedSubtree(c);
    return c;
  }
  removeChild(c) {
    if (!c) return c;
    _dom("remove_child", c._nid);
    if (globalThis.__mutationObservers?.length) globalThis.__notifyMutation('childList', this._nid, [], [c._nid]);
    return c;
  }
  replaceChild(newChild, oldChild) {
    if (!oldChild || !newChild) return oldChild;
    // A DocumentFragment is replaced by its children, in order (DOM spec).
    if (newChild.nodeType === 11 && newChild.nodeName === '#document-fragment') {
      const kids = Array.from(newChild.childNodes);
      for (const k of kids) this.insertBefore(k, oldChild);
      this.removeChild(oldChild);
      return oldChild;
    }
    _dom("insert_before", newChild._nid, oldChild._nid);
    _dom("remove_child", oldChild._nid);
    // A replacement is an insertion and a removal; an observer saw neither so far.
    if (globalThis.__mutationObservers?.length) globalThis.__notifyMutation('childList', this._nid, [newChild._nid], [oldChild._nid]);
    if (_nodeInDocument(this)) _registerIframesIn(newChild);
    __prepareInsertedSubtree(newChild);
    return oldChild;
  }
  insertBefore(n, ref) {
    if (!n) return n;
    if (!ref) { this.appendChild(n); return n; }
    if (n.nodeType === 11 && n.nodeName === '#document-fragment') {
      const kids = Array.from(n.childNodes);
      for (const k of kids) this.insertBefore(k, ref);
      return n;
    }
    _dom("insert_before", n._nid, ref._nid);
    // Same reporting as appendChild: where a node is inserted does not decide
    // whether an observer sees it.
    if (globalThis.__mutationObservers?.length) globalThis.__notifyMutation('childList', this._nid, [n._nid], []);
    if (_nodeInDocument(this)) _registerIframesIn(n);
    __prepareInsertedSubtree(n);
    return n;
  }
  contains(o) { return o ? _dom("contains", this._nid, o._nid) === "true" : false; }
  hasChildNodes() { return _dom("has_child_nodes", this._nid) === "true"; }
  cloneNode(deep) {
    const t = this.nodeType;
    if (t === 1) {
      return _wrap(+_dom("clone_node", this._nid, deep ? "true" : "false"));
    }
    // Clone structurally via real DOM nodes rather than round-tripping through a
    // throwaway <div>.innerHTML: the fragment parser discards elements that are
    // not valid children of <div> (<tr>, <td>, <option>, …), so the old path
    // returned null for them and lost JS-set inline styles. Building each node
    // directly with createElement(NS) + attribute copy avoids any parsing
    // context, and an explicit stack keeps a deep subtree from overflowing the
    // JS stack.
    const root = _shallowCloneNode(this);
    if (!deep || !root) return root;
    const stack = [[this, root]];
    while (stack.length) {
      const [src, dst] = stack.pop();
      // A <template>'s children hang off its content fragment, not childNodes,
      // so clone them into the clone's fragment. Gated on the tag name because
      // .content means something else on other elements (e.g. <meta>).
      if (src.localName === 'template' && dst.localName === 'template') {
        const sc = src.content, dc = dst.content;
        if (sc && dc && sc.childNodes) {
          const tk = sc.childNodes;
          for (let i = 0; i < tk.length; i++) {
            const c = _shallowCloneNode(tk[i]);
            if (c) { dc.appendChild(c); stack.push([tk[i], c]); }
          }
        }
      }
      const kids = src.childNodes;
      for (let i = 0; i < kids.length; i++) {
        const c = _shallowCloneNode(kids[i]);
        if (c) { dst.appendChild(c); stack.push([kids[i], c]); }
      }
    }
    return root;
  }
  compareDocumentPosition(other) {
    if (!other) return 0;
    if (this._nid === other._nid) return 0;
    // Different roots: DISCONNECTED | IMPLEMENTATION_SPECIFIC plus a stable
    // (consistent across calls) PRECEDING/FOLLOWING bit, chosen by node-id order.
    if (+_dom("node_root", this._nid) !== +_dom("node_root", other._nid)) {
      return 1 | 32 | ((this._nid < other._nid) ? 4 : 2);
    }
    if (this.contains(other)) return 16 | 4;          // CONTAINED_BY | FOLLOWING
    if (other.contains && other.contains(this)) return 8 | 2; // CONTAINS | PRECEDING
    // Same root, neither contains the other: real tree order (compare_order op:
    // -1 => this precedes other => other FOLLOWS this(4); +1 => this PRECEDING(2)).
    return (+_dom("compare_order", this._nid, other._nid) < 0) ? 4 : 2;
  }
  getRootNode() { return globalThis.document; }
  normalize() {
    // Merge adjacent exclusive Text nodes, drop empty ones, recurse. Detached
    // removed nodes keep their own data (read from the backing node by nid).
    let child = this.firstChild;
    while (child) {
      const next = child.nextSibling;
      if (child.nodeType === 3) {
        let data = child.data, sib = child.nextSibling;
        while (sib && sib.nodeType === 3) { const after = sib.nextSibling; data += sib.data; this.removeChild(sib); sib = after; }
        if (data.length === 0) { this.removeChild(child); child = sib; continue; }
        if (data !== child.data) child.data = data;
        child = sib; continue;
      } else if (child.nodeType === 1 || child.nodeType === 11) {
        child.normalize();
      }
      child = next;
    }
  }
  isEqualNode(other) {
    if (!other) return false;
    if (this._nid === other._nid) return true;
    if (this.nodeType !== other.nodeType) return false;
    if (this.nodeName !== other.nodeName) return false;
    if (this.nodeValue !== other.nodeValue) return false;
    const a = this.attributes ? this.attributes : null;
    const b = other.attributes ? other.attributes : null;
    if ((a && a.length) || (b && b.length)) {
      if (!a || !b || a.length !== b.length) return false;
      for (let i = 0; i < a.length; i++) {
        if (other.getAttribute(a[i].name) !== a[i].value) return false;
      }
    }
    const cA = this.childNodes || [];
    const cB = other.childNodes || [];
    if (cA.length !== cB.length) return false;
    for (let i = 0; i < cA.length; i++) {
      if (!cA[i].isEqualNode(cB[i])) return false;
    }
    return true;
  }
  isSameNode(other) { return other && this._nid === other._nid; }
  addEventListener() {} removeEventListener() {} dispatchEvent() { return true; }
}
class CharacterData extends Node {
  get data() {
    return _domParse("text_content", this._nid) ?? "";
  }
  set data(v) {
    const oldValue = _domParse("text_content", this._nid) ?? "";
    _dom("set_text_content", this._nid, String(v ?? ""));
    if (globalThis.__mutationObservers?.length) {
      globalThis.__notifyMutation('characterData', this._nid, [], [], null, oldValue);
    }
  }
  get length() { return this.data.length; }
  substringData(offset, count) {
    return this.data.substring(offset, offset + count);
  }
  appendData(s) { this.data += s; }
  insertData(offset, s) {
    const d = this.data;
    this.data = d.slice(0, offset) + s + d.slice(offset);
  }
  deleteData(offset, count) {
    const d = this.data;
    this.data = d.slice(0, offset) + d.slice(offset + count);
  }
  replaceData(offset, count, s) {
    const d = this.data;
    this.data = d.slice(0, offset) + s + d.slice(offset + count);
  }
}

class Text extends CharacterData {
  get nodeName() { return "#text"; }
  get nodeType() { return 3; }
  get wholeText() { return this.data; }
  splitText(offset) {
    const d = this.data;
    const tail = d.substring(offset);
    this.data = d.substring(0, offset);
    const newNid = +_dom("create_text_node", tail);
    const parent = this.parentNode;
    if (parent) {
      const ref = this.nextSibling;
      parent.insertBefore(_wrap(newNid), ref);
    }
    return _wrap(newNid);
  }
  cloneNode() { return document.createTextNode(this.data); }
}

class Comment extends CharacterData {
  get nodeName() { return "#comment"; }
  get nodeType() { return 8; }
  cloneNode() { return document.createComment(this.data); }
}

// DOMTokenList backs class/rel/sandbox/etc. attribute reflection. It parses the
// associated content attribute as an ordered set of tokens and writes changes
// straight back, so reads and writes stay live with the element. A Proxy is
// layered on top so numeric indexing (list[0]) hits item().
class DOMTokenList {
  constructor(el, attr, supportedTokens) {
    // Non-enumerable so the element <-> token-list cycle is not visible to
    // enumeration/serialization (JSON.stringify(classList) would otherwise
    // throw "circular structure").
    Object.defineProperty(this, "_el", { value: el, writable: true, enumerable: false });
    Object.defineProperty(this, "_attr", { value: attr, writable: true, enumerable: false });
    Object.defineProperty(this, "_supported", { value: supportedTokens || null, writable: true, enumerable: false });
    return new Proxy(this, {
      get(t, k, r) {
        if (typeof k === "string" && /^\d+$/.test(k)) return t.item(+k);
        return Reflect.get(t, k, r);
      },
      has(t, k) {
        if (typeof k === "string" && /^\d+$/.test(k)) return +k < t.length;
        return Reflect.has(t, k);
      },
    });
  }
  get [Symbol.toStringTag]() { return "DOMTokenList"; }
  _tokens() {
    const v = this._el.getAttribute(this._attr);
    if (!v) return [];
    const seen = new Set();
    const out = [];
    for (const tok of v.split(/[ \t\n\f\r]+/)) {
      if (tok && !seen.has(tok)) { seen.add(tok); out.push(tok); }
    }
    return out;
  }
  _write(tokens) {
    this._el.setAttribute(this._attr, tokens.join(" "));
  }
  get length() { return this._tokens().length; }
  get value() { return this._el.getAttribute(this._attr) || ""; }
  set value(v) { this._el.setAttribute(this._attr, String(v)); }
  item(i) { const t = this._tokens(); return (i >= 0 && i < t.length) ? t[i] : null; }
  contains(token) { return this._tokens().includes(String(token)); }
  add(...tokens) {
    const t = this._tokens();
    for (const raw of tokens) {
      const tok = String(raw);
      if (tok === "") throw new DOMException("The token provided must not be empty.", "SyntaxError");
      if (/[ \t\n\f\r]/.test(tok)) throw new DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError");
      if (!t.includes(tok)) t.push(tok);
    }
    this._write(t);
  }
  remove(...tokens) {
    let t = this._tokens();
    for (const raw of tokens) {
      const tok = String(raw);
      if (tok === "") throw new DOMException("The token provided must not be empty.", "SyntaxError");
      if (/[ \t\n\f\r]/.test(tok)) throw new DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError");
      t = t.filter((x) => x !== tok);
    }
    this._write(t);
  }
  toggle(token, force) {
    const tok = String(token);
    if (tok === "") throw new DOMException("The token provided must not be empty.", "SyntaxError");
    if (/[ \t\n\f\r]/.test(tok)) throw new DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError");
    const t = this._tokens();
    const has = t.includes(tok);
    if (has) {
      if (force === true) return true;
      this._write(t.filter((x) => x !== tok));
      return false;
    }
    if (force === false) return false;
    t.push(tok);
    this._write(t);
    return true;
  }
  replace(token, newToken) {
    const a = String(token), b = String(newToken);
    if (a === "" || b === "") throw new DOMException("The token provided must not be empty.", "SyntaxError");
    if (/[ \t\n\f\r]/.test(a) || /[ \t\n\f\r]/.test(b)) throw new DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError");
    const t = this._tokens();
    const i = t.indexOf(a);
    if (i === -1) return false;
    if (t.includes(b) && b !== a) { t.splice(i, 1); } else { t[i] = b; }
    this._write(t);
    return true;
  }
  supports(token) {
    if (!this._supported) throw new TypeError("DOMTokenList has no supported tokens.");
    return this._supported.includes(String(token).toLowerCase());
  }
  forEach(cb, thisArg) {
    const t = this._tokens();
    for (let i = 0; i < t.length; i++) cb.call(thisArg, t[i], i, this);
  }
  *values() { yield* this._tokens(); }
  *keys() { const t = this._tokens(); for (let i = 0; i < t.length; i++) yield i; }
  *entries() { const t = this._tokens(); for (let i = 0; i < t.length; i++) yield [i, t[i]]; }
  [Symbol.iterator]() { return this._tokens()[Symbol.iterator](); }
  toString() { return this.value; }
}

// CDATASection: a Text-derived node (nodeType 4) used only in XML documents.
// Extends Text so data/length/textContent/childNodes reuse the working text
// node machinery; only the type-identifying getters differ.
class CDATASection extends Text {
  get nodeName() { return "#cdata-section"; }
  get nodeType() { return 4; }
  get nodeValue() { return this.data; }
  set nodeValue(v) { this.data = v; }
  cloneNode() { return new CDATASection(+_dom("create_text_node", this.data)); }
}

// ProcessingInstruction: nodeType 7, nodeName === target. Extends CharacterData
// and carries a separate target. Backed by a text node so data/nodeValue/
// textContent/length work without native PI support.
class ProcessingInstruction extends CharacterData {
  constructor(nid, target) { super(nid); this._target = target; }
  get target() { return this._target; }
  get nodeName() { return this._target; }
  get nodeType() { return 7; }
  get nodeValue() { return this.data; }
  set nodeValue(v) { this.data = v; }
  cloneNode() { return new ProcessingInstruction(+_dom("create_text_node", this.data), this._target); }
}

// Document character encoding (WHATWG canonical name, e.g. "UTF-8", "EUC-JP").
// Cached per runtime: the encoding is fixed for a document's lifetime and this
// is read on every <a>/<area> URL-component access, so the UTF-8 common case
// must reduce to a single cached-boolean read with no op call and no allocation.
let __docEncoding;
let __docIsUtf8;
function _docEncoding() {
  if (__docEncoding === undefined) {
    const e = _domParse("document_encoding");
    __docEncoding = (typeof e === 'string' && e) ? e : 'UTF-8';
    __docIsUtf8 = __docEncoding.toLowerCase() === 'utf-8';
  }
  return __docEncoding;
}
function _docIsUtf8() { if (__docIsUtf8 === undefined) _docEncoding(); return __docIsUtf8; }
// WHATWG "special scheme" check (these get the special-query percent-encode set).
function _isSpecialScheme(protocol) {
  const s = (protocol || '').replace(/:$/, '').toLowerCase();
  return s === 'http' || s === 'https' || s === 'ws' || s === 'wss' || s === 'ftp' || s === 'file';
}
// Apply the WHATWG URL "encoding override": in a legacy (non-UTF-8) document
// the query of an <a>/<area> href is percent-encoded in the document charset,
// not UTF-8. The url op already produced a UTF-8-encoded query; recover the
// original characters (percent-decode + UTF-8) and re-encode them through the
// document charset. Pure-ASCII queries round-trip unchanged.
function _applyDocQueryEncoding(u) {
  if (!u || !u.search || u.search.length < 2) return u;
  let decoded;
  try { decoded = decodeURIComponent(u.search.slice(1)); } catch (e) { return u; }
  let reencoded;
  try { reencoded = _OPS.op_url_encode_query(decoded, _docEncoding(), _isSpecialScheme(u.protocol)); }
  catch (e) { return u; }
  const newSearch = '?' + reencoded;
  if (newSearch === u.search) return u;
  const hashIdx = u.href.indexOf('#');
  const frag = hashIdx >= 0 ? u.href.slice(hashIdx) : '';
  const beforeHash = hashIdx >= 0 ? u.href.slice(0, hashIdx) : u.href;
  const qIdx = beforeHash.indexOf('?');
  u.href = (qIdx >= 0 ? beforeHash.slice(0, qIdx) : beforeHash) + newSearch + frag;
  u.search = newSearch;
  return u;
}

// HTMLHyperlinkElementUtils helpers (the <a>/<area> URL-decomposition members).
// The element's href attribute is parsed against the document base URL via the
// WHATWG url op; component getters read it, setters rewrite the href attribute.
// Base = document URL with <base href> folded in (upstream #658); identity and
// origin checks keep using the plain document URL.
function _docBase() { return _domParse("document_base_url") || _domParse("document_url") || "about:blank"; }
function _anchorBase() { return _docBase(); }
function _elemHrefURL(el) {
  const raw = el.getAttribute('href');
  if (raw === null || raw === undefined) return null;
  const u = _urlParseOp(raw, _anchorBase());
  if (u && !_docIsUtf8()) return _applyDocQueryEncoding(u);
  return u;
}
function _setElemHrefPart(el, part, value) {
  const u = _elemHrefURL(el);
  if (!u) return;
  const c = _urlSetOp(u.href, part, value);
  if (c) el.setAttribute('href', c.href);
}

// --- <input> number/date conversion (valueAsNumber/valueAsDate/stepUp/Down) ---
// Applicable types and their step scale factor + default step (HTML spec).
const _INPUT_NUM_TYPES = { date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1, number: 1, range: 1 };
const _INPUT_DATE_TYPES = { date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1 };
const _INPUT_STEP_SCALE = { date: 86400000, 'datetime-local': 1000, month: 1, number: 1, range: 1, time: 1000, week: 604800000 };
const _INPUT_STEP_DEFAULT = { date: 1, 'datetime-local': 60, month: 1, number: 1, range: 1, time: 60, week: 1 };
function _pad(n, w) { n = String(Math.abs(n | 0)); while (n.length < w) n = '0' + n; return n; }
function _daysInMonth(y, m) { return [31, ((y % 4 === 0 && y % 100 !== 0) || y % 400 === 0) ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][m - 1]; }
function _isoWeek1Monday(y) { const jan4 = Date.UTC(y, 0, 4); const dow = (new Date(jan4).getUTCDay() + 6) % 7; return jan4 - dow * 86400000; }
// Parse an <input> value string to its numeric form per type; NaN if invalid.
function _inputParseNumber(type, v) {
  v = String(v == null ? '' : v);
  let m;
  switch (type) {
    case 'number': case 'range': { if (v === '') return NaN; const n = Number(v); return isFinite(n) ? n : NaN; }
    case 'date': if ((m = /^(\d{4,})-(\d{2})-(\d{2})$/.exec(v))) { const y = +m[1], mo = +m[2], d = +m[3]; if (mo >= 1 && mo <= 12 && d >= 1 && d <= _daysInMonth(y, mo)) return Date.UTC(y, mo - 1, d); } return NaN;
    case 'month': if ((m = /^(\d{4,})-(\d{2})$/.exec(v))) { const y = +m[1], mo = +m[2]; if (mo >= 1 && mo <= 12) return (y - 1970) * 12 + (mo - 1); } return NaN;
    case 'week': if ((m = /^(\d{4,})-W(\d{2})$/.exec(v))) { const y = +m[1], w = +m[2]; if (w >= 1 && w <= 53) return _isoWeek1Monday(y) + (w - 1) * 604800000; } return NaN;
    case 'time': if ((m = /^(\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)?$/.exec(v))) { const h = +m[1], mi = +m[2], s = m[3] ? +m[3] : 0, ms = m[4] ? +((m[4] + '00').slice(0, 3)) : 0; if (h <= 23 && mi <= 59 && s <= 59) return ((h * 60 + mi) * 60 + s) * 1000 + ms; } return NaN;
    case 'datetime-local': if ((m = /^(\d{4,})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)?$/.exec(v))) { const y = +m[1], mo = +m[2], d = +m[3], h = +m[4], mi = +m[5], s = m[6] ? +m[6] : 0, ms = m[7] ? +((m[7] + '00').slice(0, 3)) : 0; if (mo >= 1 && mo <= 12 && d >= 1 && d <= _daysInMonth(y, mo) && h <= 23 && mi <= 59 && s <= 59) return Date.UTC(y, mo - 1, d, h, mi, s, ms); } return NaN;
  }
  return NaN;
}
// Format a numeric value back to an <input> value string per type.
function _inputFormatNumber(type, n) {
  switch (type) {
    case 'number': case 'range': return String(n);
    case 'date': { const dt = new Date(n); return _pad(dt.getUTCFullYear(), 4) + '-' + _pad(dt.getUTCMonth() + 1, 2) + '-' + _pad(dt.getUTCDate(), 2); }
    case 'month': { const y = 1970 + Math.floor(n / 12); const mo = ((n % 12) + 12) % 12 + 1; return _pad(y, 4) + '-' + _pad(mo, 2); }
    case 'week': { const d = new Date(n); const dow = (d.getUTCDay() + 6) % 7; const thu = n - dow * 86400000 + 3 * 86400000; const ty = new Date(thu).getUTCFullYear(); const w = Math.round((n - dow * 86400000 - _isoWeek1Monday(ty)) / 604800000) + 1; return _pad(ty, 4) + '-W' + _pad(w, 2); }
    case 'time': { n = ((n % 86400000) + 86400000) % 86400000; const ms = n % 1000; n = Math.floor(n / 1000); const s = n % 60; n = Math.floor(n / 60); const mi = n % 60; const h = Math.floor(n / 60); let str = _pad(h, 2) + ':' + _pad(mi, 2); if (s || ms) { str += ':' + _pad(s, 2); if (ms) str += '.' + _pad(ms, 3); } return str; }
    case 'datetime-local': { const dt = new Date(n); let str = _pad(dt.getUTCFullYear(), 4) + '-' + _pad(dt.getUTCMonth() + 1, 2) + '-' + _pad(dt.getUTCDate(), 2) + 'T' + _pad(dt.getUTCHours(), 2) + ':' + _pad(dt.getUTCMinutes(), 2); const s = dt.getUTCSeconds(), ms = dt.getUTCMilliseconds(); if (s || ms) { str += ':' + _pad(s, 2); if (ms) str += '.' + _pad(ms, 3); } return str; }
  }
  return String(n);
}

// WebIDL interface constants live on both the interface object and the interface
// prototype object (instances inherit; idlharness checks Node.prototype).
Object.assign(Node.prototype, {
  ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4,
  ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7,
  COMMENT_NODE: 8, DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10, DOCUMENT_FRAGMENT_NODE: 11,
  NOTATION_NODE: 12, DOCUMENT_POSITION_DISCONNECTED: 1, DOCUMENT_POSITION_PRECEDING: 2,
  DOCUMENT_POSITION_FOLLOWING: 4, DOCUMENT_POSITION_CONTAINS: 8,
  DOCUMENT_POSITION_CONTAINED_BY: 16, DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: 32,
});

// HTML elements ASCII-lowercase attribute names (setAttribute('accessKey') is
// stored as 'accesskey'). The toLowerCase is gated behind a cheap uppercase
// charCode scan so the all-lowercase common case (href, class, id, data-*)
// allocates nothing and never consults the namespace; only when an uppercase
// ASCII letter is present do we check the element is HTML before folding.
function _htmlAttrName(el, n) {
  n = typeof n === "string" ? n : String(n);
  for (let i = 0; i < n.length; i++) {
    const c = n.charCodeAt(i);
    if (c >= 65 && c <= 90) {
      return el.namespaceURI === "http://www.w3.org/1999/xhtml" ? n.toLowerCase() : n;
    }
  }
  return n;
}

class Element extends Node {
  constructor(nid) {
    super(nid);
    this._style = _styleProxy(new CSSStyleDeclaration(this));
  }
  get tagName() { return _domParse("tag_name", this._nid) || ""; }
  get localName() {
    // tagName is an op call and the tag never changes, so cache the lowercased
    // localName. This keeps the new <a>/<area> href getters (which read
    // localName) and every other localName consumer off the op path.
    if (this._lname !== undefined) return this._lname;
    const ln = (this.tagName || "").toLowerCase();
    if (ln) this._lname = ln;
    return ln;
  }
  get id() { return this.getAttribute("id") || ""; }
  set id(v) { this.setAttribute("id", v); }
  get className() { return this.getAttribute("class") || ""; }
  set className(v) { this.setAttribute("class", v); }
  get namespaceURI() {
    // createElementNS records the requested namespace on _ns; an empty string
    // maps to the null namespace per spec. Elements made via createElement (or
    // parsed) have no _ns: default to XHTML, except <svg> which is SVG.
    if (this._ns !== undefined) return this._ns === "" ? null : this._ns;
    if (this.localName === "svg") return "http://www.w3.org/2000/svg";
    return "http://www.w3.org/1999/xhtml";
  }
  get innerHTML() { return _domParse("inner_html", this._nid) ?? ""; }
  set innerHTML(v) {
    if (this.localName === 'template') {
      this.content.innerHTML = v;
      return;
    }
    // Capture the children that are about to be replaced so we can deliver
    // them as `removedNodes` in the MutationObserver record. Without this,
    // libraries that mutate via `innerHTML =` (jQuery's `.html(s)`, React
    // `dangerouslySetInnerHTML`, vue-style content swaps) silently bypass
    // every MutationObserver subscriber and downstream hydration / polling
    // logic stalls.
    let oldChildren = [];
    let newChildren = [];
    if (globalThis.__mutationObservers?.length) {
      oldChildren = _domParse("child_nodes", this._nid) || [];
    }
    _dom("set_inner_html", this._nid, String(v ?? ""));
    if (_nodeInDocument(this)) _registerIframesIn(this);
    if (globalThis.__mutationObservers?.length) {
      newChildren = _domParse("child_nodes", this._nid) || [];
      globalThis.__notifyMutation('childList', this._nid, newChildren, oldChildren);
    }
  }
  get outerHTML() { return _domParse("outer_html", this._nid) ?? ""; }
  get innerText() { return this.textContent; }
  set innerText(v) { this.textContent = v; }
  get children() {
    const ids = _domParse("element_children", this._nid) || [];
    return HTMLCollection._from(ids.map(_wrapEl).filter(Boolean));
  }
  get content() {
    // <template>.content is a DocumentFragment; <meta>.content reflects
    // the content attribute (read/write per spec). Next.js' next/head
    // iterates <meta> tags and sets .content during hydration, which
    // threw with the previous getter-only stub and put React into an
    // infinite retry loop (issue #210).
    const tag = this.localName;
    if (tag === 'template') {
      // Back the fragment with the node's real template contents.
      // The parser stores template children in a separate contents document
      // instead of under the element, so without this the getter handed back a
      // fabricated empty fragment and the parsed markup was unreachable.
      // `template_contents` allocates one on demand for created templates.
      const nid = +_dom("template_contents", this._nid);
      if (nid >= 0) {
        // Cache by node id so `.content` keeps a stable identity across reads —
        // frameworks stash the fragment and compare it later.
        if (!_cache.has(nid)) _cache.set(nid, new DocumentFragment(nid));
        const content = _cache.get(nid);
        content._fragmentContext = 'template';
        return content;
      }
      if (!this._templateContent) this._templateContent = document.createDocumentFragment();
      return this._templateContent;
    }
    if (tag === 'meta') return this.getAttribute('content') || '';
    return undefined;
  }
  set content(v) {
    if (this.localName === 'meta') {
      this.setAttribute('content', v == null ? '' : String(v));
    }
  }
  get childElementCount() { return this.children.length; }
  get firstElementChild() { return this.children[0] || null; }
  get lastElementChild() { const ch = this.children; return ch[ch.length-1] || null; }
  get nextElementSibling() { let s = this.nextSibling; while(s && s.nodeType !== 1) s = s.nextSibling; return s; }
  get previousElementSibling() { let s = this.previousSibling; while(s && s.nodeType !== 1) s = s.previousSibling; return s; }
  get classList() {
    if (!this._classList) this._classList = new DOMTokenList(this, "class");
    return this._classList;
  }
  get relList() {
    const ns = this.namespaceURI, ln = this.localName;
    const ok = (ns === "http://www.w3.org/2000/svg" && ln === "a") ||
               (ns === "http://www.w3.org/1999/xhtml" && (ln === "a" || ln === "area" || ln === "link"));
    if (!ok) return undefined;
    if (!this._relList) this._relList = new DOMTokenList(this, "rel");
    return this._relList;
  }
  get sandbox() {
    if (this.namespaceURI !== "http://www.w3.org/1999/xhtml" || this.localName !== "iframe") return undefined;
    if (!this._sandboxList) this._sandboxList = new DOMTokenList(this, "sandbox");
    return this._sandboxList;
  }
  get sizes() {
    if (this.namespaceURI !== "http://www.w3.org/1999/xhtml" || this.localName !== "link") return undefined;
    if (!this._sizesList) this._sizesList = new DOMTokenList(this, "sizes");
    return this._sizesList;
  }
  get htmlFor() {
    if (this.namespaceURI !== "http://www.w3.org/1999/xhtml") return undefined;
    const ln = this.localName;
    if (ln === "output") {
      if (!this._htmlForList) this._htmlForList = new DOMTokenList(this, "for");
      return this._htmlForList;
    }
    if (ln === "label") return this.getAttribute("for") || "";
    return undefined;
  }
  set htmlFor(v) {
    if (this.namespaceURI === "http://www.w3.org/1999/xhtml" && this.localName === "label") {
      this.setAttribute("for", String(v));
    }
  }
  get style() { return this._style; }
  set style(v) { if (typeof v === "string") this._style.cssText = v; }
  getAttribute(n) {
    // Fast path: HTML attributes are stored lowercase, so a direct hit needs no
    // case folding. Only on a miss do we lowercase (gated) and retry, so the hot
    // case (reading an existing lowercase attribute) pays zero scan.
    let v = _domParse("get_attribute", this._nid, n);
    if (v === null) { const ln = _htmlAttrName(this, n); if (ln !== n) v = _domParse("get_attribute", this._nid, ln); }
    return v;
  }
  setAttribute(n, v) {
    n = _htmlAttrName(this, n);
    const popoverPrev = (n === "popover") ? this.popover : undefined;
    _dom("set_attribute", this._nid, n + "\0" + String(v));
    if (popoverPrev !== undefined) this._popoverTypeMaybeChanged(popoverPrev);
    if (globalThis.__mutationObservers?.length) globalThis.__notifyMutation('attributes', this._nid, [], [], n);
  }
  setAttributeNS(ns, n, v) { _dom("set_attribute", this._nid, String(n) + "\0" + String(v)); } // exact name, no HTML folding
  removeAttribute(n) { n = _htmlAttrName(this, n); const popoverPrev = (n === "popover") ? this.popover : undefined; _dom("remove_attribute", this._nid, n); if (popoverPrev !== undefined) this._popoverTypeMaybeChanged(popoverPrev); }
  removeAttributeNS(ns, n) { _dom("remove_attribute", this._nid, String(n)); }
  hasAttribute(n) { return this.getAttribute(n) !== null; }
  hasAttributes() { return true; } // Simplified
  get attributes() {
    const el = this;
    const names = _domParse("attribute_names", el._nid) || [];
    const list = names.map((name) => ({
      name,
      localName: name,
      value: el.getAttribute(name) ?? "",
      namespaceURI: null,
      prefix: null,
      specified: true,
      ownerElement: el,
      nodeName: name,
      nodeValue: el.getAttribute(name) ?? "",
      nodeType: 2,
    }));
    list.length = names.length;
    list.getNamedItem = (n) => names.includes(n) ? list[names.indexOf(n)] : null;
    list.setNamedItem = (a) => { if (a && a.name) el.setAttribute(a.name, a.value); return a; };
    list.removeNamedItem = (n) => { const a = list.getNamedItem(n); if (a) el.removeAttribute(n); return a; };
    list.item = (i) => list[i] || null;
    for (let i = 0; i < names.length; i++) {
      Object.defineProperty(list, names[i], { value: list[i], configurable: true, enumerable: false });
    }
    return list;
  }
  getAttributeNS(ns, n) { return _domParse("get_attribute", this._nid, String(n)); }
  querySelector(s) { return _wrapEl(+_dom("query_selector_scoped", this._nid, s)); }
  querySelectorAll(s) {
    const ids = _domParse("query_selector_all_scoped", this._nid, s) || [];
    return _nodeList(ids.map(_wrapEl).filter(Boolean));
  }
  getElementsByTagName(t) { return HTMLCollection._from(this.querySelectorAll(t)); }
  getElementsByClassName(c) { return _getElementsByClassName(this, c); }
  matches(s) {
    // :popover-open is a JS-observable popover state, not understood by the
    // native selector engine. Handle it here (and strip it from compound
    // selectors so the rest can still be matched natively).
    if (typeof s === "string" && s.indexOf(":popover-open") !== -1) {
      if (this._popoverState !== "showing") return false;
      const rest = s.replace(/:popover-open/g, "").trim();
      if (rest === "") return true;
      return this.matches(rest);
    }
    // :modal is a JS-observable dialog state (a dialog opened via showModal()),
    // not understood by the native selector engine; handle it like :popover-open.
    if (typeof s === "string" && s.indexOf(":modal") !== -1) {
      if (this._dialogModal !== true) return false;
      const rest = s.replace(/:modal/g, "").trim();
      if (rest === "") return true;
      return this.matches(rest);
    }
    const parent = this.parentNode;
    if (!parent || !parent.querySelectorAll) return false;
    const matches = parent.querySelectorAll(s);
    for (let i = 0; i < matches.length; i++) {
      if (matches[i]._nid === this._nid) return true;
    }
    return false;
  }
  closest(s) {
    let el = this;
    while (el) {
      if (el.nodeType === 1 && el.matches && el.matches(s)) return el;
      el = el.parentNode;
    }
    return null;
  }
  insertAdjacentHTML(position, html) {
    // Parse in the insertion element's context so table/select content
    // survives — a fixed <div> context makes the fragment parser drop
    // <tr>/<td>/<option> (set_inner_html parses with the target as context).
    const pos = String(position).toLowerCase();
    const parent = this.parentNode;
    const context = (pos === 'beforebegin' || pos === 'afterend') ? parent : this;
    const tag = context && context.nodeType === 1 ? String(context.tagName || 'body').toLowerCase() : 'body';
    const tmp = document.createElement(tag);
    tmp.innerHTML = html;
    // Pop firstChild repeatedly: tmp.childNodes is LIVE, so indexing it while
    // moving nodes out skips every other node.
    const nodes = [];
    let child;
    while ((child = tmp.firstChild)) nodes.push(tmp.removeChild(child));
    switch (pos) {
      case 'beforebegin':
        if (parent) { for (const n of nodes) parent.insertBefore(n, this); }
        break;
      case 'afterbegin':
        { const first = this.firstChild; for (const n of nodes) this.insertBefore(n, first); }
        break;
      case 'beforeend':
        for (const n of nodes) this.appendChild(n);
        break;
      case 'afterend':
        if (parent) { const next = this.nextSibling; for (const n of nodes) parent.insertBefore(n, next); }
        break;
      default:
        // Unknown position throws SyntaxError (a silent no-op before).
        throw new DOMException(
          "Failed to execute 'insertAdjacentHTML' on 'Element': The value provided ('" + position + "') is not one of 'beforeBegin', 'afterBegin', 'beforeEnd', or 'afterEnd'.",
          "SyntaxError"
        );
    }
  }
  // insertAdjacentElement: like insertAdjacentHTML but inserts an existing
  // element node. Some site frameworks (Zhihu's column.app.js) call this and
  // crash when it's missing, aborting the page's JS (including fingerprint
  // generation that sets __zse_ck). Implemented per DOM spec.
  insertAdjacentElement(position, element) {
    const parent = this.parentNode;
    switch (position) {
      case 'beforebegin':
        if (parent) { parent.insertBefore(element, this); return element; }
        break;
      case 'afterbegin':
        { const first = this.firstChild; this.insertBefore(element, first); return element; }
        break;
      case 'beforeend':
        this.appendChild(element);
        return element;
      case 'afterend':
        if (parent) { const next = this.nextSibling; parent.insertBefore(element, next); return element; }
        break;
    }
    return element;
  }
  addEventListener(type, handler, opts) {
    const key = this._nid;
    if (!_eventRegistry[key]) _eventRegistry[key] = {};
    if (!_eventRegistry[key][type]) _eventRegistry[key][type] = [];
    _eventRegistry[key][type].push(handler);
  }
  removeEventListener(type, handler) {
    const key = this._nid;
    if (_eventRegistry[key] && _eventRegistry[key][type]) {
      _eventRegistry[key][type] = _eventRegistry[key][type].filter(h => h !== handler);
    }
  }
  dispatchEvent(event) {
    if (!event) return true;
    if (!event.target) event.target = this;
    event.currentTarget = this;
    // Spec: inline `onclick="..."` content attributes are event handlers
    // for the matching event type. Fire them alongside any
    // addEventListener handlers. Also honor the IDL property
    // `el.onclick = fn` if set. Without this, b.click() never invokes
    // the inline handler and forms with onsubmit / buttons with onclick
    // are silently dead.
    const handlerName = 'on' + event.type;
    const inlineFn = this[handlerName] || this._resolveInlineHandler(handlerName);
    if (typeof inlineFn === 'function') {
      try {
        const ret = inlineFn.call(this, event);
        if (ret === false) event.preventDefault();
      } catch(e) { console.error(e); }
    }
    const handlers = (_eventRegistry[this._nid] || {})[event.type] || [];
    for (const h of handlers) {
      try { h.call(this, event); } catch(e) { console.error(e); }
      if (event._immediatePropagationStopped) break;
    }
    if (event.bubbles && !event._propagationStopped && this.parentNode) {
      this.parentNode.dispatchEvent(event);
    }
    return !event.defaultPrevented;
  }
  _resolveInlineHandler(name) {
    // name = 'onclick' / 'onsubmit' / etc. Compile the content attribute
    // as a function body on first read and cache it on the instance.
    const cache = this.__inlineHandlerCache || (this.__inlineHandlerCache = {});
    if (Object.prototype.hasOwnProperty.call(cache, name)) return cache[name];
    const src = this.getAttribute && this.getAttribute(name);
    if (!src) { cache[name] = null; return null; }
    try {
      cache[name] = new Function('event', src);
    } catch (e) {
      cache[name] = null;
    }
    return cache[name];
  }
  click() {
    // Disabled form controls have no activation behaviour and dispatch no
    // click event at all — own disabled attribute or disabled by an ancestor
    // <fieldset disabled> (first <legend> exempt).
    if (_isFormControlDisabled(this)) return;
    // "Click in progress" flag per spec, checked BEFORE any pre-activation
    // step: a nested .click() on an element whose click is still running must
    // be a full no-op, not a second state flip. This also stops a control's
    // handler clicking its own label from bouncing the click back.
    if (this._ditingClickInProgress) return;
    this._ditingClickInProgress = true;
    try {
      // Pre-click activation steps (HTML spec): a checkbox/radio flips BEFORE the
      // click event dispatches, so listeners observe the new state, and the flip
      // reverts if the event is cancelled. Radio groups uncheck same-name peers
      // up front; a cancel restores every prior state. Checkbox activation also
      // clears `indeterminate`; a cancel restores it along with `checked`.
      const tag = this.tagName;
      const type = (this.getAttribute('type') || '').toLowerCase();
      const checkable = tag === 'INPUT' && (type === 'checkbox' || type === 'radio');
      let oldChecked = false, oldIndeterminate = false, radioStates = null;
      if (checkable) {
        oldChecked = !!this.checked;
        if (type === 'radio') {
          const name = this.getAttribute('name') || '';
          if (name) {
            radioStates = [];
            const all = (this.ownerDocument || globalThis.document).querySelectorAll('input');
            for (let i = 0; i < all.length; i++) {
              const r = all[i];
              if ((r.getAttribute('type') || '').toLowerCase() !== 'radio') continue;
              if ((r.getAttribute('name') || '') !== name || r.form !== this.form) continue;
              radioStates.push([r, !!r.checked]);
              if (r !== this) r.checked = false;
            }
          }
          this.checked = true;
        } else {
          this.checked = !oldChecked;
          oldIndeterminate = !!this.indeterminate;
          this.indeterminate = false;
        }
      }
      const cancelled = !this.dispatchEvent(new MouseEvent("click", {bubbles: true, cancelable: true}));
      if (cancelled) {
        if (radioStates) { for (let i = 0; i < radioStates.length; i++) radioStates[i][0].checked = radioStates[i][1]; }
        else if (checkable) {
          this.checked = oldChecked;
          if (type === 'checkbox') this.indeterminate = oldIndeterminate;
        }
        return;
      }
      if (checkable && this.checked !== oldChecked) {
        try { this.dispatchEvent(new Event('input', {bubbles: true})); } catch (e) {}
        try { this.dispatchEvent(new Event('change', {bubbles: true})); } catch (e) {}
        return;
      }
      // Label activation behaviour (HTML spec): activating a label runs a
      // synthetic click on its labeled control. Interactive elements keep
      // their own activation, so a control nested inside a label toggles
      // once instead of forwarding through the label and firing twice.
      const selfInteractive = this.matches && this.matches(_LABELABLE + ',a');
      const labelHost = selfInteractive ? null : (tag === 'LABEL' ? this : (this.closest ? this.closest('label') : null));
      if (labelHost) {
        const control = _labeledControl(labelHost);
        // A disabled control has no activation behaviour — own attribute or
        // disabled through an ancestor fieldset both count.
        if (control && control !== this && !_isFormControlDisabled(control)) {
          control.click();
          return;
        }
      }
      const link = tag === 'A' ? this : (this.closest ? this.closest('a[href]') : null);
      if (link) {
        const href = link.getAttribute('href');
        if (href && !href.startsWith('#') && !href.startsWith('javascript:')) {
          location.assign(href);
          return;
        }
      }
      if (_isSubmitButton(this)) {
        const form = this.closest ? this.closest('form') : null;
        if (form) {
          if (typeof form.requestSubmit === 'function') {
            form.requestSubmit(this);
          } else if (typeof form.submit === 'function') {
            form.submit(this);
          }
        }
      }
    } finally {
      this._ditingClickInProgress = false;
    }
  }
  focus() { globalThis.__diting_focused = this; globalThis.__diting_click_target = this; }
  blur() { if (globalThis.__diting_focused === this) globalThis.__diting_focused = null; }

  // --- Popover API (HTML "popover") ---------------------------------------
  // Read the popover content attribute case-insensitively. The HTML parser
  // lowercases attribute names, but runtime setAttribute("PoPoVeR", ...)
  // preserves case, and the IDL reflection matches the name ASCII-case-
  // insensitively. Returns the raw stored string, or null if absent.
  _popoverAttrValue() {
    const v = this.getAttribute("popover");
    if (v !== null) return v;
    const names = _domParse("attribute_names", this._nid) || [];
    for (let i = 0; i < names.length; i++) {
      if (names[i].toLowerCase() === "popover") return this.getAttribute(names[i]);
    }
    return null;
  }
  // The reflected (effective) popover type: null (No Popover), "auto",
  // "hint", or "manual". Empty string maps to "auto"; any non-keyword value
  // (invalid) maps to "manual".
  get popover() {
    const raw = this._popoverAttrValue();
    if (raw === null) return null;
    const v = String(raw).toLowerCase();
    if (v === "auto" || v === "hint" || v === "manual") return v;
    if (v === "") return "auto";
    return "manual";
  }
  set popover(value) {
    if (value === null || value === undefined) { this._popoverRemoveAttr(); return; }
    this.setAttribute("popover", String(value));
  }
  _popoverRemoveAttr() {
    if (this.getAttribute("popover") !== null) { this.removeAttribute("popover"); return; }
    const names = _domParse("attribute_names", this._nid) || [];
    for (let i = 0; i < names.length; i++) {
      if (names[i].toLowerCase() === "popover") { this.removeAttribute(names[i]); return; }
    }
  }
  // "check popover validity". expectedToBeShowing is true for hide, false for
  // show. Throws NotSupportedError when there is no valid popover type, and
  // InvalidStateError when the element is not connected; returns false (no
  // throw) when the current state does not match expectedToBeShowing.
  _checkPopoverValidity(expectedToBeShowing) {
    if (this.popover === null) throw new DOMException("Not supported on elements that don't have a valid value for the popover attribute", "NotSupportedError");
    const showing = this._popoverState === "showing";
    if ((expectedToBeShowing && !showing) || (!expectedToBeShowing && showing)) return false;
    if (!this.isConnected) throw new DOMException("Invalid on popover elements which aren't connected", "InvalidStateError");
    return true;
  }
  showPopover() {
    if (!this._checkPopoverValidity(/*expectedToBeShowing*/false)) return;
    const beforeEvent = new ToggleEvent("beforetoggle", { cancelable: true, oldState: "closed", newState: "open" });
    if (!this.dispatchEvent(beforeEvent)) return;
    // The beforetoggle handler may have changed our type or shown us; re-check.
    if (!this._checkPopoverValidity(/*expectedToBeShowing*/false)) return;
    this._popoverState = "showing";
    const target = this;
    setTimeout(() => { try { target.dispatchEvent(new ToggleEvent("toggle", { oldState: "closed", newState: "open" })); } catch (e) {} }, 0);
  }
  hidePopover() {
    if (!this._checkPopoverValidity(/*expectedToBeShowing*/true)) return;
    this.dispatchEvent(new ToggleEvent("beforetoggle", { oldState: "open", newState: "closed" }));
    this._popoverState = "hidden";
    const target = this;
    setTimeout(() => { try { target.dispatchEvent(new ToggleEvent("toggle", { oldState: "open", newState: "closed" })); } catch (e) {} }, 0);
  }
  togglePopover(force) {
    let options = force;
    if (options && typeof options === "object") force = options.force;
    const showing = this._popoverState === "showing";
    if (showing && (force === undefined || force === null || force === false)) {
      this.hidePopover();
    } else if (force === undefined || force === null || force === true) {
      this.showPopover();
    }
    return this._popoverState === "showing";
  }
  // Called from setAttribute/removeAttribute/IDL setter when the popover
  // attribute may have changed. If the effective type changed while showing,
  // hide the popover (firing the hide events) per the HTML spec.
  _popoverTypeMaybeChanged(prevType) {
    const newType = this.popover;
    if (this._popoverState === "showing" && prevType !== newType) {
      // Hide directly. Do not call hidePopover(): it re-validates against the
      // popover attribute, which may now be removed (No Popover), and would
      // throw NotSupportedError. This mirrors the spec hide with throw=false.
      this.dispatchEvent(new ToggleEvent("beforetoggle", { oldState: "open", newState: "closed" }));
      this._popoverState = "hidden";
      const target = this;
      setTimeout(() => { try { target.dispatchEvent(new ToggleEvent("toggle", { oldState: "open", newState: "closed" })); } catch (e) {} }, 0);
    }
  }
  // HTMLDialogElement members (live on Element.prototype like popover/input;
  // meaningful only when localName === 'dialog'). Modal top-layer/focus/render
  // is layout (out of scope); the open state, returnValue, and beforetoggle/
  // toggle/close/cancel events are JS-observable and implemented here.
  get open() { return this.hasAttribute('open'); }
  set open(v) { if (v) { if (!this.hasAttribute('open')) this.setAttribute('open', ''); } else if (this.hasAttribute('open')) { this.removeAttribute('open'); this._dialogModal = false; } }
  get returnValue() { return this._returnValue != null ? this._returnValue : ''; }
  set returnValue(v) { this._returnValue = String(v); }
  get oncancel() { return this._oncancel || null; }
  set oncancel(f) { this._oncancel = typeof f === 'function' ? f : null; }
  get onclose() { return this._onclose || null; }
  set onclose(f) { this._onclose = typeof f === 'function' ? f : null; }
  get closedBy() { const v = (this.getAttribute('closedby') || '').toLowerCase(); return (v === 'any' || v === 'closerequest' || v === 'none') ? v : 'auto'; }
  set closedBy(v) { this.setAttribute('closedby', String(v)); }
  show() {
    if (this.hasAttribute('open')) { if (this._dialogModal) throw new DOMException("The dialog is already open as a modal dialog.", "InvalidStateError"); return; }
    const before = new ToggleEvent("beforetoggle", { cancelable: true, oldState: "closed", newState: "open" });
    if (!this.dispatchEvent(before)) return;
    if (this.hasAttribute('open')) return;
    this.setAttribute('open', ''); this._dialogModal = false;
    const self = this; setTimeout(() => { try { self.dispatchEvent(new ToggleEvent("toggle", { oldState: "closed", newState: "open" })); } catch (e) {} }, 0);
  }
  showModal() {
    if (this.hasAttribute('open')) throw new DOMException("The dialog is already open.", "InvalidStateError");
    if (!this.isConnected) throw new DOMException("The dialog is not connected to a document.", "InvalidStateError");
    const before = new ToggleEvent("beforetoggle", { cancelable: true, oldState: "closed", newState: "open" });
    if (!this.dispatchEvent(before)) return;
    if (this.hasAttribute('open')) return;
    this.setAttribute('open', ''); this._dialogModal = true;
    const self = this; setTimeout(() => { try { self.dispatchEvent(new ToggleEvent("toggle", { oldState: "closed", newState: "open" })); } catch (e) {} }, 0);
  }
  _dialogClose(result, fireClose) {
    if (!this.hasAttribute('open')) return;
    this.dispatchEvent(new ToggleEvent("beforetoggle", { oldState: "open", newState: "closed" }));
    this.removeAttribute('open'); this._dialogModal = false;
    if (result !== undefined) this._returnValue = String(result);
    const self = this;
    setTimeout(() => { try { self.dispatchEvent(new ToggleEvent("toggle", { oldState: "open", newState: "closed" })); } catch (e) {} }, 0);
    if (fireClose) setTimeout(() => { try { self.dispatchEvent(new Event('close', { bubbles: false, cancelable: false })); } catch (e) {} }, 0);
  }
  close(result) { this._dialogClose(result, true); }
  requestClose(result) {
    if (!this.hasAttribute('open')) return;
    if (this._dialogCancelFiring) return; // no re-entrant cancel
    this._dialogCancelFiring = true;
    let canceled = false;
    try { const ev = new Event('cancel', { bubbles: false, cancelable: true }); this.dispatchEvent(ev); canceled = ev.defaultPrevented; }
    finally { this._dialogCancelFiring = false; }
    if (canceled) return;
    this._dialogClose(result, true);
  }
  attachInternals() {
    const reg = (typeof customElements !== 'undefined' && customElements._registry) ? customElements._registry : null;
    if (!reg || !reg.get(this.localName)) throw new DOMException("Failed to execute 'attachInternals' on 'HTMLElement': Unable to attach ElementInternals to non-custom elements.", "NotSupportedError");
    if (this.getAttribute('is')) throw new DOMException("Failed to execute 'attachInternals' on 'HTMLElement': Unable to attach ElementInternals to a customized built-in element.", "NotSupportedError");
    if (this._internalsAttached) throw new DOMException("Failed to execute 'attachInternals' on 'HTMLElement': ElementInternals for the specified element was already attached.", "NotSupportedError");
    this._internalsAttached = true;
    return new ElementInternals(this);
  }
  get value() {
    const tag = this.localName;
    if (tag === 'select') {
      // Selected option wins; otherwise first option (HTML default).
      const opts = this.querySelectorAll('option');
      for (let i = 0; i < opts.length; i++) {
        if (opts[i].selected) {
          return opts[i].getAttribute('value') !== null ? opts[i].getAttribute('value') : opts[i].textContent;
        }
      }
      if (opts.length) return opts[0].getAttribute('value') !== null ? opts[0].getAttribute('value') : opts[0].textContent;
      return '';
    }
    if (_formValues[this._nid] !== undefined) return _formValues[this._nid];
    if (tag === 'textarea') return this.textContent;
    if (tag === 'option') {
      const attr = this.getAttribute('value');
      return attr !== null ? attr : this.textContent;
    }
    if (tag === 'input') {
      const itype = (this.getAttribute('type') || '').toLowerCase();
      if (itype === 'checkbox' || itype === 'radio') {
        // A checkbox/radio with no value attribute defaults to "on" in a real
        // browser, not the empty string.
        const attr = this.getAttribute('value');
        return attr !== null ? attr : 'on';
      }
    }
    return this.getAttribute("value") || "";
  }
  set value(v) {
    const tag = this.localName;
    if (tag === 'option') {
      this.setAttribute('value', String(v));
      return;
    }
    if (tag === 'select') {
      // Set selected on matching option, clear on others. Puppeteer's
      // page.select(selector, value) round-trips through this setter and
      // dispatches its own input/change events in-page afterwards, like a
      // real browser: a programmatic value assignment never fires change
      // itself. Dispatching here fed pages that assign inside a change
      // handler back into that handler in an infinite loop.
      const wanted = String(v);
      const opts = this.querySelectorAll('option');
      for (let i = 0; i < opts.length; i++) {
        const attrV = opts[i].getAttribute('value');
        const optVal = attrV !== null ? attrV : opts[i].textContent;
        opts[i].selected = optVal === wanted;
      }
      return;
    }
    _formValues[this._nid] = String(v);
    if (tag === 'textarea') {
      this.textContent = String(v);
    }
  }
  get min() { return this.getAttribute('min') || ''; }
  set min(v) { this.setAttribute('min', v); }
  get max() { return this.getAttribute('max') || ''; }
  set max(v) { this.setAttribute('max', v); }
  get step() { return this.getAttribute('step') || ''; }
  set step(v) { this.setAttribute('step', v); }
  _inputType() { return this.localName === 'input' ? (this.getAttribute('type') || 'text').toLowerCase() : ''; }
  get valueAsNumber() {
    const t = this._inputType();
    if (!_INPUT_NUM_TYPES[t]) return NaN;
    if (t === 'range') {
      let minN = _inputParseNumber('range', this.getAttribute('min')); if (isNaN(minN)) minN = 0;
      let maxN = _inputParseNumber('range', this.getAttribute('max')); if (isNaN(maxN)) maxN = 100;
      if (maxN < minN) maxN = minN;
      const v = _inputParseNumber('range', this.value);
      let n = isNaN(v) ? (minN + (maxN - minN) / 2) : v;
      if (n < minN) n = minN; if (n > maxN) n = maxN;
      return n;
    }
    return _inputParseNumber(t, this.value);
  }
  set valueAsNumber(n) {
    const t = this._inputType();
    if (!_INPUT_NUM_TYPES[t]) throw new DOMException("Failed to set the 'valueAsNumber' property on 'HTMLInputElement': This input element does not support Number values.", 'InvalidStateError');
    n = Number(n);
    if (isNaN(n)) { this.value = ''; return; }
    if (!isFinite(n)) throw new TypeError("Failed to set the 'valueAsNumber' property on 'HTMLInputElement': The value provided is infinite.");
    this.value = _inputFormatNumber(t, n);
  }
  get valueAsDate() {
    const t = this._inputType();
    if (!_INPUT_DATE_TYPES[t]) return null;
    const n = _inputParseNumber(t, this.value);
    if (isNaN(n)) return null;
    if (t === 'month') { const y = 1970 + Math.floor(n / 12); const mo = ((n % 12) + 12) % 12; return new Date(Date.UTC(y, mo, 1)); }
    return new Date(n);
  }
  set valueAsDate(d) {
    const t = this._inputType();
    if (!_INPUT_DATE_TYPES[t]) throw new DOMException("Failed to set the 'valueAsDate' property on 'HTMLInputElement': This input element does not support Date values.", 'InvalidStateError');
    if (d === null) { this.value = ''; return; }
    if (!(d instanceof Date)) throw new TypeError("Failed to set the 'valueAsDate' property on 'HTMLInputElement': The provided value is not a Date.");
    const ms = d.getTime();
    if (isNaN(ms)) { this.value = ''; return; }
    if (t === 'month') { this.value = _inputFormatNumber('month', (d.getUTCFullYear() - 1970) * 12 + d.getUTCMonth()); return; }
    this.value = _inputFormatNumber(t, ms);
  }
  stepUp(n) { this._stepBy(n === undefined ? 1 : (n | 0)); }
  stepDown(n) { this._stepBy(-(n === undefined ? 1 : (n | 0))); }
  _stepBy(delta) {
    const t = this._inputType();
    const stepAttr = this.getAttribute('step');
    if (!_INPUT_STEP_SCALE[t] || (stepAttr && stepAttr.trim().toLowerCase() === 'any')) {
      throw new DOMException("Failed to execute 'stepUp' on 'HTMLInputElement': This form element does not have allowed value steps.", 'InvalidStateError');
    }
    const scale = _INPUT_STEP_SCALE[t];
    let stepN = _INPUT_STEP_DEFAULT[t];
    if (stepAttr) { const s = Number(stepAttr); if (isFinite(s) && s > 0) stepN = s; }
    const allowed = stepN * scale;
    const minN = _inputParseNumber(t, this.getAttribute('min'));
    const maxN = _inputParseNumber(t, this.getAttribute('max'));
    const stepBase = isNaN(minN) ? 0 : minN;
    let value = this.valueAsNumber;
    if (isNaN(value)) value = isNaN(minN) ? 0 : minN;
    value += delta * allowed;
    value = stepBase + Math.round((value - stepBase) / allowed) * allowed;
    const effMin = (t === 'range' && isNaN(minN)) ? 0 : minN;
    const effMax = (t === 'range' && isNaN(maxN)) ? 100 : maxN;
    if (!isNaN(effMin) && value < effMin) value = effMin;
    if (!isNaN(effMax) && value > effMax) value = effMax;
    this.value = _inputFormatNumber(t, value);
  }
  get checked() {
    if (_formChecked[this._nid] !== undefined) return _formChecked[this._nid];
    return this.hasAttribute("checked");
  }
  set checked(v) { _formChecked[this._nid] = !!v; }
  // Real IDL property (not an expando): `in` checks, Object.keys enumeration,
  // and prototype introspection must all see it, and the click activation
  // steps clear/restore it as real state.
  get indeterminate() { return !!_formIndeterminate[this._nid]; }
  set indeterminate(v) { _formIndeterminate[this._nid] = !!v; }
  get selected() {
    if (this._selected !== undefined) return this._selected;
    return this.hasAttribute("selected");
  }
  set selected(v) { this._selected = !!v; }
  get disabled() { return this.hasAttribute("disabled"); }
  set disabled(v) { if (v) this.setAttribute("disabled", ""); else this.removeAttribute("disabled"); }
  get type() {
    // select and textarea report fixed IDL types, not the content attribute.
    // jQuery's select valHook branches on type === "select-one" to decide
    // scalar vs array .val(); "" here made every single select read as an
    // array, so value comparisons against strings never matched.
    if (this.localName === "select") return this.hasAttribute("multiple") ? "select-multiple" : "select-one";
    if (this.localName === "textarea") return "textarea";
    return this.getAttribute("type") || (this.localName === "input" ? "text" : "");
  }
  set type(v) { this.setAttribute("type", v); }
  get name() { return this.getAttribute("name") || ""; }
  set name(v) { this.setAttribute("name", v); }
  get placeholder() { return this.getAttribute("placeholder") || ""; }
  set placeholder(v) { this.setAttribute("placeholder", v); }
  // For <a>/<area>, href returns the resolved absolute URL (the spec behavior,
  // and what scrapers want). It uses op_url_resolve, which returns just the
  // resolved string, rather than the full-component op the decomposition
  // members use. Other elements reflect the raw attribute.
  get href() {
    const ln = this.localName;
    if (ln === 'a' || ln === 'area' || ln === 'link' || ln === 'base') {
      const raw = this.getAttribute('href');
      if (raw === null) return '';
      // Legacy-charset document: href must reflect the encoding-override query.
      if (!_docIsUtf8()) { const u = _elemHrefURL(this); return u ? u.href : raw; }
      const r = _urlResolveOp(raw, _anchorBase());
      return r !== null ? r : raw;
    }
    return this.getAttribute("href") || "";
  }
  set href(v) { this.setAttribute("href", v); }
  // HTMLHyperlinkElementUtils URL-decomposition members, live on <a>/<area>.
  get protocol() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.protocol : ''; }
  set protocol(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'protocol', v); }
  get username() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.username : ''; }
  set username(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'username', v); }
  get password() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.password : ''; }
  set password(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'password', v); }
  get host() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.host : ''; }
  set host(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'host', v); }
  get hostname() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.hostname : ''; }
  set hostname(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'hostname', v); }
  get port() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.port : ''; }
  set port(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'port', v); }
  get pathname() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.pathname : ''; }
  set pathname(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'pathname', v); }
  get search() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.search : ''; }
  set search(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'search', v); }
  get hash() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.hash : ''; }
  set hash(v) { if (this.localName === 'a' || this.localName === 'area') _setElemHrefPart(this, 'hash', v); }
  get origin() { const u = (this.localName === 'a' || this.localName === 'area') ? _elemHrefURL(this) : null; return u ? u.origin : ''; }
  get src() {
    const raw = this.getAttribute("src");
    // Spec: missing attribute reflects as "". An empty attribute resolves
    // against the document base (so `<img src="">.src` returns the document
    // URL) — don't special-case '' away before resolution.
    if (raw === null) return '';
    // URL-reflection attribute (HTML spec): return the resolved absolute URL,
    // not the raw attribute string. Real browsers resolve `img.src`,
    // `script.src`, `iframe.src` etc. against the document base. Next.js /
    // Turbopack's webpack runtime derives its chunk base from
    // `document.currentScript.src` via `new URL(x, base)`; a relative base
    // throws "TypeError: Invalid scheme" and React never hydrates (all
    // delegated event listeners are dead). Resolve like real browsers.
    const r = _urlResolveOp(raw, _anchorBase());
    return r !== null ? r : raw;
  }
  set src(v) {
    this.setAttribute("src", v);
    if (this.localName === 'iframe' && v && v !== 'about:blank') {
      this._loadIframeSrc(v);
    }
  }
  _loadIframeSrc(url) {
    let fullUrl = url;
    if (!url.includes('://')) {
      try { fullUrl = new URL(url, _docBase()).href; } catch(e) {}
    }
    const el = this;
    fetch(fullUrl, {mode: 'no-cors'}).then(async resp => {
      if (resp.ok || resp.type === 'opaque') {
        const html = await resp.text();
        el._iframeDoc = new _IframeDocument(html, fullUrl, el);
        el._iframeWin = new _IframeWindow(el._iframeDoc, fullUrl);
      } else {
        el._iframeDoc = new _IframeDocument('<!DOCTYPE html><html><head></head><body></body></html>', fullUrl, el);
        el._iframeWin = new _IframeWindow(el._iframeDoc, fullUrl);
      }
      _registerIframe(el);
      // Dispatch through the element so the onload property/attribute and any
      // addEventListener('load', ...) listeners all run. Calling el.onload()
      // directly bypasses listeners registered via addEventListener (#478).
      el.dispatchEvent(new Event('load'));
    }).catch(() => {
      el._iframeDoc = new _IframeDocument('<!DOCTYPE html><html><head></head><body></body></html>', fullUrl, el);
      el._iframeWin = new _IframeWindow(el._iframeDoc, fullUrl);
      _registerIframe(el);
      el.dispatchEvent(new Event('load'));
    });
  }
  get contentDocument() {
    if (this.localName !== 'iframe') return undefined;
    if (this._iframeDoc) {
      const pageOrigin = (function(){ try { return new URL(_domParse("document_url")).origin; } catch(e) { return ''; } })();
      const iframeOrigin = (function(url){ try { return new URL(url).origin; } catch(e) { return ''; } })(this.src);
      if (pageOrigin === iframeOrigin || this.src === '' || this.src === 'about:blank' || !this.src.includes('://')) {
        return this._iframeDoc;
      }
      return null; // Cross-origin: blocked
    }
    if (!this._iframeDoc) {
      // Lazy creation before any load: derive the browsing context's URL
      // from the src attribute when present (browsers do this at context
      // creation), so contentWindow.location.origin and postMessage
      // targetOrigin checks see the authored origin, not about:blank.
      const lazyUrl = (this.src && this.src.includes('://')) ? this.src : 'about:blank';
      this._iframeDoc = new _IframeDocument('<!DOCTYPE html><html><head></head><body></body></html>', lazyUrl, this);
      this._iframeWin = new _IframeWindow(this._iframeDoc, lazyUrl);
    }
    return this._iframeDoc;
  }
  get contentWindow() {
    if (this.localName !== 'iframe') return undefined;
    if (!this._iframeWin) {
      this.contentDocument; // side effect: creates _iframeDoc + _iframeWin
    }
    return this._iframeWin;
  }
  get action() {
    const action = this.getAttribute("action") || _docBase();
    try { return new URL(action, _docBase()).href; } catch(e) { return action; }
  }
  set action(v) { this.setAttribute("action", v); }
  get method() { return this.getAttribute("method") || "get"; }
  set method(v) { this.setAttribute("method", v); }
  get form() {
    let p = this.parentNode;
    while (p && p.localName !== 'form') p = p.parentNode;
    return p;
  }
  get options() {
    if (this.localName !== 'select') return [];
    return HTMLCollection._from(this.querySelectorAll('option'));
  }
  add(item, before = null) {
    if (this.localName !== 'select') {
      throw new TypeError("Illegal invocation");
    }
    if (!item || item.nodeType !== 1
        || (item.localName !== 'option' && item.localName !== 'optgroup')) {
      throw new TypeError("Failed to execute 'add' on 'HTMLSelectElement': parameter 1 is not of type 'HTMLOptionElement' or 'HTMLOptGroupElement'.");
    }
    if (typeof before === 'number') {
      const reference = this.options[before] || null;
      this.insertBefore(item, reference);
    } else if (before == null) {
      this.appendChild(item);
    } else {
      this.insertBefore(item, before);
    }
  }
  get selectedIndex() {
    const opts = this.options;
    for (let i = 0; i < opts.length; i++) {
      if (opts[i].selected || opts[i].hasAttribute('selected')) return i;
    }
    // Only a single select implicitly selects its first option; a multiple
    // select with nothing chosen idles at -1 like a real browser.
    return opts.length && !this.hasAttribute('multiple') ? 0 : -1;
  }
  set selectedIndex(v) {
    const opts = this.options;
    for (let i = 0; i < opts.length; i++) {
      opts[i]._selected = (i === v);
    }
  }
  // Per the HTML spec, the submit() METHOD submits the form WITHOUT firing a
  // cancelable `submit` event — a page's submit listener cannot veto it. Only
  // requestSubmit() and user-initiated submits fire the cancelable event.
  // Conflating the two broke sites whose submit listener preventDefault()s the
  // native submit and then calls form.submit() from a callback (e.g. an
  // invisible-reCAPTCHA data-callback) to actually send the form.
  submit(submitter) {
    this._navigateSubmit(submitter);
  }
  requestSubmit(submitter) {
    // Per spec, a given submitter must be a submit button owned by this form;
    // both checks run before the submit event fires. A missing/null submitter
    // means "submit from the form itself".
    if (submitter !== undefined && submitter !== null) {
      if (!_isSubmitButton(submitter)) {
        throw new TypeError(
          "Failed to execute 'requestSubmit' on 'HTMLFormElement': The specified element is not a submit button."
        );
      }
      if (submitter.form !== this) {
        throw new DOMException(
          "Failed to execute 'requestSubmit' on 'HTMLFormElement': The specified element is not owned by this form element.",
          'NotFoundError'
        );
      }
    }
    const cancelled = !this.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    if (cancelled) return;
    this._navigateSubmit(submitter);
  }
  _navigateSubmit(submitter) {
    const pairs = [];
    const fields = this.querySelectorAll('input, select, textarea');
    for (let i = 0; i < fields.length; i++) {
      const f = fields[i];
      const name = f.getAttribute('name');
      if (!name) continue;
      if (f.getAttribute('disabled') !== null) continue;
      const tag = f.localName;
      const type = (f.getAttribute('type') || '').toLowerCase();
      if ((type === 'checkbox' || type === 'radio') && !f.checked) continue;
      if (type === 'file' || type === 'reset') continue;
      if (type === 'button') continue;
      if (type === 'submit' || tag === 'button') {
        if (submitter && f !== submitter) continue;
        if (!submitter) continue; // default submit: don't include submit button value
      }

      let val;
      if (tag === 'select') {
        const opt = f.querySelector('option[selected]') || f.querySelector('option');
        val = opt ? (opt.getAttribute('value') !== null ? opt.getAttribute('value') : opt.textContent) : '';
      } else if (tag === 'textarea') {
        val = f.value || f.textContent || '';
      } else {
        val = f.value !== undefined ? f.value : (f.getAttribute('value') || '');
      }
      const enc = (s) => encodeURIComponent(s).replace(/%20/g, '+').replace(/!/g, '%21');
      pairs.push(enc(name) + '=' + enc(val));
    }

    const action = this.getAttribute('action') || '';
    const method = (this.getAttribute('method') || 'GET').toUpperCase();
    const baseUrl = globalThis.location?.href || 'about:blank';
    let targetUrl;
    try { targetUrl = new URL(action, baseUrl).href; } catch(e) { targetUrl = action; }

    const encoded = pairs.join('&');
    if (method === 'POST') {
      _OPS.op_navigate(targetUrl, 'POST', encoded);
    } else {
      const sep = targetUrl.includes('?') ? '&' : '?';
      _OPS.op_navigate(targetUrl + (encoded ? sep + encoded : ''), 'GET', '');
    }
  }
  reset() {
    this.dispatchEvent(new Event('reset', { bubbles: true }));
  }
  get dataset() {
    const el = this;
    if (el._dataset) return el._dataset;
    const attrFor = (k) => "data-" + String(k).replace(/([A-Z])/g, "-$1").toLowerCase();
    const camel = (n) => n.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    const dataKeys = () => (_domParse("attribute_names", el._nid) || [])
      .filter((n) => n.startsWith("data-"))
      .map(camel);
    // Proxy target is a real DOMStringMap so `dataset instanceof DOMStringMap`
    // and the [object DOMStringMap] tag hold; data-* reflection stays dynamic
    // (upstream ec05ed0).
    el._dataset = new Proxy(new DOMStringMap(_domStringMapKey), {
      get(target, k, receiver) {
        if (typeof k === "string" && el.hasAttribute(attrFor(k))) return el.getAttribute(attrFor(k));
        return Reflect.get(target, k, receiver);
      },
      set(target, k, v, receiver) {
        if (typeof k !== "string") return Reflect.set(target, k, v, receiver);
        el.setAttribute(attrFor(k), String(v));
        return true;
      },
      has(target, k) {
        return (typeof k === "string" && el.hasAttribute(attrFor(k))) || Reflect.has(target, k);
      },
      deleteProperty(target, k) {
        if (typeof k !== "string") return Reflect.deleteProperty(target, k);
        el.removeAttribute(attrFor(k));
        return true;
      },
      ownKeys() { return dataKeys(); },
      getOwnPropertyDescriptor(target, k) {
        if (typeof k === "string" && el.hasAttribute(attrFor(k))) {
          return { value: el.getAttribute(attrFor(k)), writable: true, enumerable: true, configurable: true };
        }
        return Reflect.getOwnPropertyDescriptor(target, k);
      },
    });
    return el._dataset;
  }
  get offsetWidth() {
    if (this._isViewportRoot()) return (globalThis.innerWidth || 1280);
    const m = _obscuraFontBox(this);
    return m ? m.w : 100;
  }
  get offsetHeight() {
    if (this._isViewportRoot()) return (globalThis.innerHeight || 720);
    const m = _obscuraFontBox(this);
    return m ? m.h : 20;
  }
  get offsetTop() { return 0; } get offsetLeft() { return 0; }
  // documentElement / body / window expose VIEWPORT geometry, not their own content box.
  // Puppeteer's #clickableBox clips boxes to document.documentElement.clientWidth/Height;
  // returning 100x20 there made every element appear off-screen and broke .click().
  get clientWidth() { return this._isViewportRoot() ? (globalThis.innerWidth || 1280) : 100; }
  get clientHeight() { return this._isViewportRoot() ? (globalThis.innerHeight || 720) : 20; }
  get scrollWidth() { return this._isViewportRoot() ? (globalThis.innerWidth || 1280) : 100; }
  get scrollHeight() { return this._isViewportRoot() ? (globalThis.innerHeight || 720) : 20; }
  _isViewportRoot() {
    const t = this.tagName;
    return t === 'HTML' || t === 'BODY';
  }
  // No layout engine, so there is no real overflow to scroll and the offset is
  // deliberately NOT clamped: without real geometry any synthetic max is a
  // guess, and a max derived from a stub scroll box pins scrollTop at 0, which
  // deadlocks scroll-driven lazy loaders (no scroll -> no content -> no scroll).
  // We track the offset so scrollTop/scrollLeft round-trip, and fire a scroll
  // event on direct assignment — lazy loaders that set `el.scrollTop = N` rely
  // on that event, and scrollTo/scrollBy below would otherwise be its only source.
  get scrollTop() { return this._scrollTop || 0; }
  set scrollTop(v) {
    v = +v;
    const nv = Number.isFinite(v) && v > 0 ? v : 0;
    const changed = nv !== (this._scrollTop || 0);
    this._scrollTop = nv;
    if (changed && !this._scrollSuppress) this._fireScroll();
  }
  get scrollLeft() { return this._scrollLeft || 0; }
  set scrollLeft(v) {
    v = +v;
    const nv = Number.isFinite(v) && v > 0 ? v : 0;
    const changed = nv !== (this._scrollLeft || 0);
    this._scrollLeft = nv;
    if (changed && !this._scrollSuppress) this._fireScroll();
  }
  getBoundingClientRect() {
    globalThis.__diting_click_target = this;
    // documentElement and body span the full viewport. Without this every
    // hit test against them clips down to a 100x20 synthetic cell and
    // Document.elementFromPoint can never recurse into their children.
    if (this._isViewportRoot()) {
      const vw = globalThis.innerWidth || 1280;
      const vh = globalThis.innerHeight || 720;
      return {
        x: 0, y: 0, width: vw, height: vh,
        top: 0, right: vw, bottom: vh, left: 0,
        toJSON() { return this; },
      };
    }
    // Real layout first: the diting_css + diting_layout pipeline computes
    // actual geometry for this tree (memoized per tree epoch in the op
    // layer). Falls back to the synthetic grid when the layout feature is
    // compiled out or the element has no box (display:none subtree, etc.).
    if (this._nid != null) {
      try {
        const raw = _domRaw("layout_rect", String(this._nid | 0), "");
        const arr = typeof raw === "string" ? JSON.parse(raw) : raw;
        if (Array.isArray(arr) && arr.length === 4 && Number.isFinite(arr[0])) {
          const [x, y, w, h] = arr;
          return {
            x, y, width: w, height: h,
            top: y, right: x + w, bottom: y + h, left: x,
            toJSON() { return this; },
          };
        }
      } catch (e) { /* fall through to synthetic */ }
    }
    // No layout engine, but Playwright's actionability polling needs each
    // element to occupy a stable, distinct rect so hit-testing can pick the
    // right one (issue #45). Synthesize a deterministic position from the
    // node id: every nid maps to a unique cell in a 12-column grid, sized
    // to fit a 1280x720 viewport. Stable across reads, different per node.
    const VW = 1280, VH = 720, COLS = 12, CW = 100, CH = 20, GX = 110, GY = 30;
    const rowsPerScreen = Math.max(1, Math.floor((VH - 10) / GY));
    const cell = this._nid | 0;
    const col = ((cell * 7) | 0) % COLS;
    const row = (((cell * 13) | 0) >> 0) % rowsPerScreen;
    const x = 10 + col * GX;
    const y = 10 + row * GY;
    return {
      x, y, width: CW, height: CH,
      top: y, right: x + CW, bottom: y + CH, left: x,
      toJSON() { return this; },
    };
  }
  getClientRects() { return [this.getBoundingClientRect()]; }
  // No layout engine: a stub that always returns true unblocks Playwright's
  // actionability polling. With a real layout we'd check display, visibility,
  // opacity and rect dimensions per spec.
  checkVisibility(opts) { return true; }
  // ARIA reflection properties. Without an accessibility tree we expose the
  // raw aria-* attributes so Playwright's getByRole / getByLabel locators can
  // at least find elements that author them explicitly.
  get role() { return this.getAttribute('role'); }
  set role(v) { if (v == null) this.removeAttribute('role'); else this.setAttribute('role', String(v)); }
  get ariaLabel() { return this.getAttribute('aria-label'); }
  set ariaLabel(v) { if (v == null) this.removeAttribute('aria-label'); else this.setAttribute('aria-label', String(v)); }
  get ariaRoleDescription() { return this.getAttribute('aria-roledescription'); }
  set ariaRoleDescription(v) { if (v == null) this.removeAttribute('aria-roledescription'); else this.setAttribute('aria-roledescription', String(v)); }
  get ariaChecked() { return this.getAttribute('aria-checked'); }
  set ariaChecked(v) { if (v == null) this.removeAttribute('aria-checked'); else this.setAttribute('aria-checked', String(v)); }
  get ariaDisabled() { return this.getAttribute('aria-disabled'); }
  set ariaDisabled(v) { if (v == null) this.removeAttribute('aria-disabled'); else this.setAttribute('aria-disabled', String(v)); }
  get ariaExpanded() { return this.getAttribute('aria-expanded'); }
  set ariaExpanded(v) { if (v == null) this.removeAttribute('aria-expanded'); else this.setAttribute('aria-expanded', String(v)); }
  get ariaHidden() { return this.getAttribute('aria-hidden'); }
  set ariaHidden(v) { if (v == null) this.removeAttribute('aria-hidden'); else this.setAttribute('aria-hidden', String(v)); }
  get ariaSelected() { return this.getAttribute('aria-selected'); }
  set ariaSelected(v) { if (v == null) this.removeAttribute('aria-selected'); else this.setAttribute('aria-selected', String(v)); }
  scrollIntoView() { globalThis.__diting_click_target = this; }
  // scrollTo/scrollBy/scroll accept either (x, y) or a ScrollToOptions object.
  // Without layout the offset cannot be clamped to a real max, but updating it
  // and firing a scroll event lets scroll-driven lazy loaders advance instead
  // of throwing "scrollBy is not a function" (#429). The setters fire a scroll
  // event of their own, so suppress the per-axis ones here and emit a single
  // event for the whole movement, the way a real browser coalesces one scroll
  // per scroll operation rather than one per axis.
  scrollTo(x, y) {
    let left, top;
    if (x !== null && typeof x === 'object') { left = x.left; top = x.top; }
    else { left = x; top = y; }
    this._scrollSuppress = true;
    if (left !== undefined) this.scrollLeft = +left || 0;
    if (top !== undefined) this.scrollTop = +top || 0;
    this._scrollSuppress = false;
    this._fireScroll();
  }
  scroll(x, y) { this.scrollTo(x, y); }
  scrollBy(x, y) {
    let dl, dt;
    if (x !== null && typeof x === 'object') { dl = x.left; dt = x.top; }
    else { dl = x; dt = y; }
    this._scrollSuppress = true;
    this.scrollLeft = (this.scrollLeft || 0) + (+dl || 0);
    this.scrollTop = (this.scrollTop || 0) + (+dt || 0);
    this._scrollSuppress = false;
    this._fireScroll();
  }
  _fireScroll() {
    const self = this;
    setTimeout(() => { try { self.dispatchEvent(new Event('scroll', { bubbles: false })); } catch (e) {} }, 0);
  }
  animate(keyframes, options) {
    const duration = typeof options === 'number' ? options : (options?.duration || 0);
    return {
      finished: Promise.resolve(), currentTime: 0, playState: 'finished',
      effect: { getComputedTiming() { return { duration }; } },
      cancel(){}, finish(){}, play(){}, pause(){}, reverse(){},
      addEventListener(){}, removeEventListener(){},
      onfinish: null, oncancel: null,
    };
  }
  getAnimations() { return []; }
  get isConnected() {
    var node = this;
    while (node) {
      if (node.nodeType === 9) return true;
      node = node.parentNode;
    }
    return false;
  }
  remove() { if (this.parentNode) this.parentNode.removeChild(this); }
  append(...nodes) { for (const n of _convertNodes(nodes)) this.appendChild(n); }
  prepend(...nodes) {
    const ref = this.firstChild;
    for (const n of _convertNodes(nodes)) {
      if (ref) this.insertBefore(n, ref); else this.appendChild(n);
    }
  }
  replaceChildren(...nodes) {
    const converted = _convertNodes(nodes);
    let c;
    while ((c = this.firstChild)) this.removeChild(c);
    for (const n of converted) this.appendChild(n);
  }
}

// WHATWG "convert nodes into a node": a Node argument passes through, anything
// else is stringified into a Text node, so e.g. append(null) inserts the text
// "null" and append(undefined) inserts "undefined" per the (Node or DOMString)
// union, rather than throwing.
function _convertNodes(nodes) {
  const out = [];
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i];
    if (n && typeof n._nid === "number") out.push(n);
    else out.push(document.createTextNode(String(n)));
  }
  return out;
}

// ---- Reflected IDL attributes (WHATWG) ---------------------------------------
// Installed ONCE on Element.prototype as shared getter/setter pairs. This is
// data-driven so there is no per-element defineProperty: element creation and
// the querySelector/mutation hot paths are unaffected (each access is a normal
// prototype getter that reads the backing attribute). Covers the global content
// attributes reflected on every element plus the ARIAMixin (aria-* + ariaXxx).
(function installElementReflectors() {
  const P = Element.prototype;
  const def = (name, get, set) => {
    if (Object.prototype.hasOwnProperty.call(P, name)) return; // never clobber an existing member
    Object.defineProperty(P, name, { get, set, enumerable: true, configurable: true });
  };
  // WHATWG "rules for parsing integers"; returns a JS number or null on failure.
  const parseIntAttr = (s) => {
    if (s === null || s === undefined) return null;
    const m = /^[ \t\n\f\r]*([+-]?[0-9]+)/.exec(String(s));
    if (!m) return null;
    const n = parseInt(m[1], 10);
    return Number.isFinite(n) ? n : null;
  };
  // IDL `long` conversion (ToInt32): finite, truncated, wrapped to 32-bit signed.
  const toLong = (v) => {
    let n = Number(v);
    if (!Number.isFinite(n)) n = 0;
    n = Math.trunc(n) % 4294967296;
    if (n >= 2147483648) n -= 4294967296;
    else if (n < -2147483648) n += 4294967296;
    return n;
  };
  // DOMString reflect: get -> attribute or ""; set -> setAttribute(String(v)).
  const reflectStr = (name, attr) => def(name,
    function () { const v = this.getAttribute(attr); return v === null ? "" : v; },
    function (v) { this.setAttribute(attr, String(v)); });
  // boolean reflect: get -> hasAttribute; set -> truthy ? add("") : remove.
  const reflectBool = (name, attr) => def(name,
    function () { return this.hasAttribute(attr); },
    function (v) { if (v) this.setAttribute(attr, ""); else this.removeAttribute(attr); });
  // long reflect: get -> parse else default (static value or per-element fn);
  // set -> setAttribute(String(ToInt32(v))).
  const reflectLong = (name, attr, dflt) => def(name,
    function () {
      const r = parseIntAttr(this.getAttribute(attr));
      if (r !== null && r >= -2147483648 && r <= 2147483647) return r;
      return typeof dflt === "function" ? dflt.call(this) : dflt;
    },
    function (v) { this.setAttribute(attr, String(toLong(v))); });
  // enumerated reflect: get -> canonical (lowercased) keyword, else missing/
  // invalid default; set -> setAttribute(String(v)) (canonicalization on get).
  const reflectEnum = (name, attr, keywords, missingDefault, invalidDefault) => def(name,
    function () {
      const v = this.getAttribute(attr);
      if (v === null) return missingDefault;
      const lc = String(v).toLowerCase();
      return keywords.indexOf(lc) !== -1 ? lc : invalidDefault;
    },
    function (v) { this.setAttribute(attr, String(v)); });
  // nullable DOMString reflect (ARIA): get -> attribute or null; set -> null/
  // undefined removes, else setAttribute(String(v)).
  const reflectNullable = (name, attr) => def(name,
    function () { return this.getAttribute(attr); },
    function (v) { if (v === null || v === undefined) this.removeAttribute(attr); else this.setAttribute(attr, String(v)); });

  // Global content attributes reflected on every element (HTML "global attributes").
  reflectStr("title", "title");
  reflectStr("lang", "lang");
  reflectStr("accessKey", "accesskey");
  reflectStr("slot", "slot");
  reflectEnum("dir", "dir", ["ltr", "rtl", "auto"], "", "");
  reflectBool("autofocus", "autofocus");
  reflectBool("hidden", "hidden");
  // tabIndex default is element-dependent (0 for natively-focusable, else -1);
  // reflection.js does not assert it, but match the common case anyway.
  reflectLong("tabIndex", "tabindex", function () {
    const ln = this.localName;
    if (ln === "a" || ln === "area" || ln === "link") return this.hasAttribute("href") ? 0 : -1;
    return (ln === "button" || ln === "input" || ln === "select" || ln === "textarea" || ln === "iframe") ? 0 : -1;
  });

  // ARIAMixin: aria-* content attributes reflected as nullable DOMString IDL
  // properties (ariaAtomic <-> aria-atomic, ...).
  const ARIA = {
    ariaAtomic: "aria-atomic", ariaAutoComplete: "aria-autocomplete", ariaBrailleLabel: "aria-braillelabel",
    ariaBrailleRoleDescription: "aria-brailleroledescription", ariaBusy: "aria-busy", ariaChecked: "aria-checked",
    ariaColCount: "aria-colcount", ariaColIndex: "aria-colindex", ariaColIndexText: "aria-colindextext",
    ariaColSpan: "aria-colspan", ariaCurrent: "aria-current", ariaDescription: "aria-description",
    ariaDisabled: "aria-disabled", ariaExpanded: "aria-expanded", ariaHasPopup: "aria-haspopup",
    ariaHidden: "aria-hidden", ariaInvalid: "aria-invalid", ariaKeyShortcuts: "aria-keyshortcuts",
    ariaLabel: "aria-label", ariaLevel: "aria-level", ariaLive: "aria-live", ariaModal: "aria-modal",
    ariaMultiLine: "aria-multiline", ariaMultiSelectable: "aria-multiselectable", ariaOrientation: "aria-orientation",
    ariaPlaceholder: "aria-placeholder", ariaPosInSet: "aria-posinset", ariaPressed: "aria-pressed",
    ariaReadOnly: "aria-readonly", ariaRelevant: "aria-relevant", ariaRequired: "aria-required",
    ariaRoleDescription: "aria-roledescription", ariaRowCount: "aria-rowcount", ariaRowIndex: "aria-rowindex",
    ariaRowIndexText: "aria-rowindextext", ariaRowSpan: "aria-rowspan", ariaSelected: "aria-selected",
    ariaSetSize: "aria-setsize", ariaSort: "aria-sort", ariaValueMax: "aria-valuemax",
    ariaValueMin: "aria-valuemin", ariaValueNow: "aria-valuenow", ariaValueText: "aria-valuetext",
  };
  for (const prop in ARIA) reflectNullable(prop, ARIA[prop]);
})();

class Document extends Node {
  get documentElement() { return _wrapEl(+_dom("document_element")); }
  get head() { return this.querySelector("head"); }
  get body() { return this.querySelector("body"); }
  get doctype() {
    if (this._doctype !== undefined) return this._doctype;
    const info = _domParse("document_doctype");
    if (info && info.name) {
      this._doctype = new DocumentType(info.nodeId, info.name, info.publicId || "", info.systemId || "");
    } else {
      this._doctype = null;
    }
    return this._doctype;
  }
  get title() { return _domParse("document_title") ?? ""; }
  // Spec: setting document.title updates the <title> element's text, creating
  // one under <head> if missing. The op keeps the Rust-side title (used for
  // navigation responses) in sync; the DOM write keeps querySelector('title')
  // and outerHTML consistent with what the page set.
  set title(v) {
    v = String(v);
    _dom("set_document_title", v);
    const head = this.head;
    if (!head) return;
    let el = head.querySelector("title");
    if (!el) { el = this.createElement("title"); head.appendChild(el); }
    el.textContent = v;
  }
  get URL() { return _domParse("document_url") ?? ""; }
  get documentURI() { return this.URL; }
  // URL of the document that initiated this navigation; empty for direct
  // automation navigations. Computed by the navigation layer per
  // strict-origin-when-cross-origin (upstream edb1785).
  get referrer() { return _domParse("document_referrer") ?? ""; }
  get location() { return globalThis.location; }
  set location(url) { _OPS.op_navigate(_resolveUrl(String(url)), 'GET', ''); }
  get defaultView() { return globalThis; }
  get nodeType() { return 9; }
  get nodeName() { return "#document"; }
  get ownerDocument() { return null; } // Document has no ownerDocument
  get compatMode() { return "CSS1Compat"; }
  // The document's character encoding, detected from the response charset
  // (HTTP Content-Type -> <meta charset>). characterSet/charset/inputEncoding
  // are WHATWG aliases. A node-less document (DOMParser/createDocument) has no
  // backing encoding and reports UTF-8.
  get characterSet() { return (this._nid === undefined || this._nid === null) ? "UTF-8" : _docEncoding(); }
  get charset() { return this.characterSet; }
  get inputEncoding() { return this.characterSet; }
  get contentType() {
    // An explicit type set by DOMParser/createDocument wins.
    if (this._contentType) return this._contentType;
    // `new Document()` (the WHATWG constructor, no backing node id) creates an
    // XML document, so createCDATASection/etc. must not throw. Live documents
    // wrapped from the tree carry a real nid and fall through to URL-derived.
    if (this._nid === undefined || this._nid === null) return "application/xml";
    const url = this.URL || "";
    // data: URLs carry their MIME type explicitly.
    const dm = /^data:([^,;]+)/i.exec(url);
    if (dm) {
      const mime = dm[1].toLowerCase();
      if (mime === "application/xhtml+xml") return "application/xhtml+xml";
      if (mime === "text/xml") return "text/xml";
      if (mime === "application/xml" || mime.endsWith("+xml")) return "application/xml";
    }
    if (/\.xhtml(?:[?#]|$)/i.test(url)) return "application/xhtml+xml";
    if (/\.(?:xml|svg)(?:[?#]|$)/i.test(url)) return "application/xml";
    return "text/html";
  }
  get readyState() { return globalThis.__documentReadyState__ || 'complete'; }
  get currentScript() {
    // Next.js / Turbopack chunk loader reads document.currentScript.src to
    // derive its base path. page.rs sets __currentScriptNid before each
    // <script> body runs and clears it after, mirroring real Chrome.
    const nid = globalThis.__currentScriptNid;
    return nid ? _wrapEl(+nid) : null;
  }
  get hidden() { return false; }
  get visibilityState() { return "visible"; }
  getElementById(id) { return _wrapEl(+_dom("get_element_by_id", id)); }
  querySelector(s) { return _wrapEl(+_dom("query_selector", s)); }
  querySelectorAll(s) {
    const ids = _domParse("query_selector_all", s) || [];
    return _nodeList(ids.map(_wrapEl).filter(Boolean));
  }
  getElementsByTagName(t) { return HTMLCollection._from(this.querySelectorAll(t)); }
  getElementsByClassName(c) { return _getElementsByClassName(this, c); }
  getElementsByName(name) { return this.querySelectorAll('[name="' + String(name).replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"]'); }
  createElement(t) {
    const el = _wrapEl(+_dom("create_element", t.toLowerCase()));
    if (el && t.toLowerCase() === 'template') {
      el._templateContent = this.createDocumentFragment();
    }
    return el;
  }
  createElementNS(ns, t) {
    const el = this.createElement(t);
    if (el) el._ns = ns;
    return el;
  }
  createTextNode(t) { return _wrap(+_dom("create_text_node", String(t))); }
  createComment(t) {
    const nid = +_dom("create_comment_node", String(t ?? ""));
    const n = new Comment(nid);
    _cache.set(nid, n);
    return n;
  }
  createCDATASection(data) {
    // Spec: throw NotSupportedError on an HTML document, reject data
    // containing "]]>", then return a CDATASection node.
    if (!_isXMLDocument(this)) {
      throw new DOMException("createCDATASection is not supported in HTML documents", "NotSupportedError");
    }
    const str = String(data);
    if (str.indexOf("]]>") !== -1) {
      throw new DOMException("CDATA section data must not contain ']]>'", "InvalidCharacterError");
    }
    const nid = +_dom("create_text_node", str);
    const n = new CDATASection(nid);
    _cache.set(nid, n);
    return n;
  }
  createProcessingInstruction(target, data) {
    // Spec: not gated on document type. Reject targets that are not an XML
    // Name, then reject data containing "?>", then return a PI node.
    const tgt = String(target);
    const str = String(data);
    if (!_isValidPITarget(tgt)) {
      throw new DOMException("Invalid processing instruction target", "InvalidCharacterError");
    }
    if (str.indexOf("?>") !== -1) {
      throw new DOMException("Processing instruction data must not contain '?>'", "InvalidCharacterError");
    }
    const nid = +_dom("create_text_node", str);
    const n = new ProcessingInstruction(nid, tgt);
    _cache.set(nid, n);
    return n;
  }
  createDocumentFragment() {
    const nid = +_dom("create_document_fragment");
    const frag = new DocumentFragment(nid);
    _cache.set(nid, frag);
    return frag;
  }
  // Legacy DOM Level 2 event factory. Spec returns an event of the requested
  // class with an empty type until init*Event() is called. We previously
  // returned a generic Event for every type, which broke libraries that call
  // createEvent('CustomEvent').initCustomEvent(...) — see issue #41.
  createEvent(type) {
    const eventType = String(type || '');
    const normalized = eventType.toLowerCase();
    const map = {
      'event': Event, 'events': Event,
      'htmlevents': Event, 'svgevents': Event,
      'customevent': CustomEvent, 'customevents': CustomEvent,
      'mouseevent': MouseEvent,   'mouseevents': MouseEvent,
      'keyboardevent': KeyboardEvent, 'keyboardevents': KeyboardEvent,
      'focusevent': FocusEvent,
      'hashchangeevent': HashChangeEvent,
      'inputevent': InputEvent,
      'messageevent': MessageEvent,
      'uievent': UIEvent, 'uievents': UIEvent,
      'compositionevent': CompositionEvent,
      'wheelevent': WheelEvent,
      'pointerevent': PointerEvent,
      'errorevent': ErrorEvent,
      'popstateevent': PopStateEvent,
      'animationevent': AnimationEvent,
      'transitionevent': TransitionEvent,
      'storageevent': StorageEvent,
    };
    // Chrome throws NotSupportedError for unknown interface names — silently
    // returning a generic Event hides typos from callers ('NotARealType' would
    // come back as an Event whose init* methods are all missing). Note
    // 'promiserejectionevent' is intentionally absent: Chrome rejects it too.
    const Cls = map[normalized];
    if (!Cls) {
      throw new DOMException(
        `The provided event type ('${eventType}') is invalid`,
        'NotSupportedError'
      );
    }
    return new Cls('');
  }
  createRange() { return new Range(); }
  addEventListener(type, fn, opts) {
    if (typeof fn !== 'function') return;
    if (!this._listeners) this._listeners = {};
    if (!this._listeners[type]) this._listeners[type] = [];
    if (!this._listeners[type].includes(fn)) this._listeners[type].push(fn);
  }
  removeEventListener(type, fn) {
    if (this._listeners?.[type]) {
      this._listeners[type] = this._listeners[type].filter(h => h !== fn);
    }
  }
  dispatchEvent(event) {
    if (!event) return true;
    const handlers = (this._listeners?.[event.type] || []).slice();
    for (const h of handlers) { try { h.call(this, event); } catch(e) { console.error('document event error:', e); } }
    return !event.defaultPrevented;
  }
  createTreeWalker(root, whatToShow, filter) {
    // whatToShow is unsigned long; default SHOW_ALL only when the arg is omitted.
    // An explicit 0 (show nothing) must stay 0, not become SHOW_ALL.
    whatToShow = (whatToShow === undefined) ? 0xFFFFFFFF : (whatToShow >>> 0);
    const walker = {
      root: root,
      currentNode: root,
      whatToShow: whatToShow,
      filter: filter || null,
      // Three-valued per NodeFilter: 1 ACCEPT, 2 REJECT, 3 SKIP. REJECT and
      // SKIP both mean "don't return this node", but only REJECT prunes its
      // descendants, so nextNode() needs to tell them apart (issue #461).
      // A node filtered out by whatToShow is a SKIP: the spec never consults
      // the filter for it, and its descendants stay eligible.
      _filter(node) {
        const nodeType = node.nodeType;
        if (!((whatToShow >> (nodeType - 1)) & 1)) return 3;
        if (this.filter) {
          if (typeof this.filter === 'function') return this.filter(node);
          if (this.filter.acceptNode) return this.filter.acceptNode(node);
        }
        return 1;
      },
      _accept(node) { return this._filter(node) === 1; },
      nextNode() {
        let node = _wrap(+_dom("next_in_subtree", this.root._nid, this.currentNode._nid));
        while (node) {
          const verdict = this._filter(node);
          if (verdict === 1) { this.currentNode = node; return node; }
          // FILTER_REJECT skips the node AND its subtree; FILTER_SKIP (and any
          // other non-accept value) skips only the node.
          const step = verdict === 2 ? "next_after_subtree" : "next_in_subtree";
          node = _wrap(+_dom(step, this.root._nid, node._nid));
        }
        return null;
      },
      // DOM 6.1 "previousNode", implemented as specified (issue #462). The old
      // version looked at exactly one candidate — the previous sibling's
      // deepest last child — and returned null the moment it was filtered out,
      // so a backward walk died mid-tree the way nextNode used to before #432.
      //
      // Unlike nextNode this stays in JS rather than using a DOM traversal op:
      // the descent into last children has to stop on FILTER_REJECT, so the
      // filter is consulted at every step anyway and there is no run of
      // crossings for a native helper to collapse.
      previousNode() {
        let node = this.currentNode;
        while (node !== this.root) {
          let sibling = node.previousSibling;
          while (sibling) {
            node = sibling;
            let verdict = this._filter(node);
            // Descend to the deepest last descendant, but never into a rejected
            // subtree — that is what makes REJECT prune backwards as well.
            while (verdict !== 2 && node.lastChild) {
              node = node.lastChild;
              verdict = this._filter(node);
            }
            if (verdict === 1) { this.currentNode = node; return node; }
            sibling = node.previousSibling;
          }
          const parent = node.parentNode;
          // Reaching root (or a detached node) ends the walk: root is never
          // returned by a backward traversal.
          if (!parent || node === this.root) return null;
          node = parent;
          if (node === this.root) return null;
          if (this._filter(node) === 1) { this.currentNode = node; return node; }
        }
        return null;
      },
      // DOM 6.1 "traverse children" (issue #469). The movers used to step
      // straight to the next sibling when a node was not accepted, so a
      // FILTER_SKIP node hid its children instead of exposing them. `edge` and
      // `step` pick the direction: first/next for forward, last/previous for
      // backward.
      _traverseChildren(edge, step) {
        let node = this.currentNode[edge];
        while (node) {
          const verdict = this._filter(node);
          if (verdict === 1) { this.currentNode = node; return node; }
          // Only SKIP leaves the children eligible; REJECT prunes the subtree.
          if (verdict === 3) {
            const child = node[edge];
            if (child) { node = child; continue; }
          }
          // Subtree exhausted: step sideways, climbing out without passing
          // root or the node the walk started from.
          while (node) {
            const sibling = node[step];
            if (sibling) { node = sibling; break; }
            const parent = node.parentNode;
            if (!parent || parent === this.root || parent === this.currentNode) return null;
            node = parent;
          }
        }
        return null;
      },
      // DOM 6.1 "traverse siblings" (issue #469).
      _traverseSiblings(edge, step) {
        let node = this.currentNode;
        if (node === this.root) return null;
        for (;;) {
          let sibling = node[step];
          while (sibling) {
            node = sibling;
            const verdict = this._filter(node);
            if (verdict === 1) { this.currentNode = node; return node; }
            // Descend into a skipped sibling's subtree; a rejected one is
            // off-limits, and a childless one has nothing to descend into.
            sibling = node[edge];
            if (verdict === 2 || !sibling) sibling = node[step];
          }
          node = node.parentNode;
          if (!node || node === this.root) return null;
          // An accepted parent is where the walk would go next, so there is no
          // sibling to return.
          if (this._filter(node) === 1) return null;
        }
      },
      firstChild() { return this._traverseChildren('firstChild', 'nextSibling'); },
      lastChild() { return this._traverseChildren('lastChild', 'previousSibling'); },
      nextSibling() { return this._traverseSiblings('firstChild', 'nextSibling'); },
      previousSibling() { return this._traverseSiblings('lastChild', 'previousSibling'); },
      // DOM 6.1 "parentNode" (issue #475). The old version looked only at the
      // immediate parent, so it couldn't climb past a skipped ancestor; it also
      // excluded `root` as a result yet stepped to root's own parent when
      // currentNode was root, returning a node OUTSIDE the walker's subtree.
      // The loop's `node !== this.root` guard is what keeps the walk inside
      // root while still allowing root itself to be returned.
      parentNode() {
        let node = this.currentNode;
        while (node && node !== this.root) {
          node = node.parentNode;
          if (node && this._accept(node)) { this.currentNode = node; return node; }
        }
        return null;
      },
    };
    return walker;
  }
  // A real NodeIterator (DOM 6.2), not a TreeWalker in disguise (issue #467).
  // The two differ in more than naming: an iterator's pointer starts *before*
  // its root, so the first nextNode() returns the root itself, and it exposes
  // referenceNode/pointerBeforeReferenceNode/detach rather than a TreeWalker's
  // currentNode and child/sibling movers.
  createNodeIterator(root, whatToShow, filter) {
    // whatToShow is unsigned long; default SHOW_ALL only when the arg is
    // omitted. An explicit 0 (show nothing) must stay 0, not become SHOW_ALL.
    whatToShow = (whatToShow === undefined) ? 0xFFFFFFFF : (whatToShow >>> 0);
    return {
      root: root,
      referenceNode: root,
      pointerBeforeReferenceNode: true,
      whatToShow: whatToShow,
      filter: filter || null,
      // NodeIterator prunes nothing: FILTER_REJECT behaves as FILTER_SKIP, so
      // unlike the TreeWalker only "accepted or not" matters here.
      _accept(node) {
        if (!((whatToShow >> (node.nodeType - 1)) & 1)) return false;
        if (this.filter) {
          if (typeof this.filter === 'function') return this.filter(node) === 1;
          if (this.filter.acceptNode) return this.filter.acceptNode(node) === 1;
        }
        return true;
      },
      // DOM 6.2 "traverse". The pointer sits either before or after
      // referenceNode, which is why reversing direction re-yields the current
      // node instead of stepping over it.
      _traverse(forward) {
        let node = this.referenceNode;
        let before = this.pointerBeforeReferenceNode;
        for (;;) {
          if (forward === before) {
            // Consume the pointer's side without moving: it flips to the other
            // side of the node it already references.
            before = !before;
          } else {
            const step = forward ? "next_in_subtree" : "prev_in_subtree";
            const next = _wrap(+_dom(step, this.root._nid, node._nid));
            // A failed traversal leaves referenceNode and the pointer
            // untouched, so the iterator can be resumed in either direction.
            if (!next) return null;
            node = next;
          }
          if (this._accept(node)) break;
        }
        this.referenceNode = node;
        this.pointerBeforeReferenceNode = before;
        return node;
      },
      nextNode() { return this._traverse(true); },
      previousNode() { return this._traverse(false); },
      // Legacy no-op since DOM4, but older library code still calls it and
      // used to hit "detach is not a function".
      detach() {},
    };
  }
  getSelection() { return this.defaultView ? _selectionFor(this) : null; }
  get activeElement() { return globalThis.__diting_focused || this.body; }
  // The element that scrolls the viewport, and where the page offset lives.
  // Standards mode, so documentElement — quirks mode would be body, but we
  // never parse in quirks mode (upstream #468).
  get scrollingElement() { return this.documentElement; }
  get implementation() {
    const ownerDoc = this;
    return {
      // Spec: createHTMLDocument returns a NEW detached Document. jQuery
      // 3.x's selector feature-detect calls `body.innerHTML = '<form>'` on
      // the result — when we returned `globalThis.document`, the real
      // `<body>` was wiped, taking every page on the open web that ships
      // jQuery 3.x with it. Reuse the DOMParser path to build a detached
      // document, then optionally set the title.
      createHTMLDocument(title) {
        // Build head>title and body explicitly. Parsing a full skeleton string
        // as innerHTML of <html> collapses through the fragment parser (it
        // dropped head/body and kept only <title>), leaving doc.body null.
        const doc = new DOMParser().parseFromString("", "text/html");
        const root = doc.documentElement;
        const head = document.createElement("head");
        const titleEl = document.createElement("title");
        if (title != null) titleEl.textContent = String(title);
        head.appendChild(titleEl);
        const body = document.createElement("body");
        root.appendChild(head);
        root.appendChild(body);
        return doc;
      },
      // Real spec: createDocument(namespaceURI, qualifiedName, doctype) →
      // an XML document with a root element of the given name. We don't
      // have a separate XML stack, so return a minimal detached document
      // with an element of the requested local name as documentElement.
      createDocument(_ns, qualifiedName, _doctype) {
        const name = (qualifiedName && String(qualifiedName)) || "root";
        const safe = name.replace(/[^a-zA-Z0-9-]/g, "");
        const html = qualifiedName ? `<${safe}></${safe}>` : "";
        const doc = new DOMParser().parseFromString(html, "application/xml");
        if (_doctype) doc._docType = _doctype;
        return doc;
      },
      // createDocumentType(qualifiedName, publicId, systemId): build a detached
      // DocumentType node. Browsers validate leniently here (only a name with
      // ASCII whitespace or ">" is rejected, matching the WPT cases); the node's
      // owner document is the document whose implementation was used.
      createDocumentType(qualifiedName, publicId, systemId) {
        const name = String(qualifiedName);
        if (name === "" || /[\t\n\f\r >]/.test(name)) {
          throw new DOMException("The qualified name '" + name + "' contains an invalid character", "InvalidCharacterError");
        }
        const dt = new DocumentType(
          +_dom("create_comment_node", ""),
          name,
          publicId === undefined ? "" : String(publicId),
          systemId === undefined ? "" : String(systemId)
        );
        dt._ownerDocument = ownerDoc;
        return dt;
      },
      hasFeature() { return true; },
    };
  }
  get styleSheets() { return []; }
  get forms() { return this.querySelectorAll("form"); }
  get images() { return this.querySelectorAll("img"); }
  get links() { return this.querySelectorAll("a[href], area[href]"); }
  get scripts() { return this.querySelectorAll("script"); }
  get cookie() {
    return _OPS.op_get_cookies();
  }
  set cookie(v) {
    if (!v) return;
    _OPS.op_set_cookie(v);
  }
  // Inserts into the document's input stream, which the host keeps alive across calls.
  // Parsing each call on its own would lose every construct that spans two of them — a
  // tag may be split anywhere, even mid tag-name (SAP UI5's cachebuster writes exactly
  // that way: one call for "<script", one per attribute, then ">").
  // https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-document-write
  write(...args) {
    var html = args.join('');
    if (!html) return;
    var body = this.body;
    if (!body) return;
    // The host parses into the input stream and returns [[parent, node], …], parents first.
    // The insertion stays here, because appendChild does more than append: it reports the
    // mutation and runs a written script.
    var placements = _domParse("document_write", "", html) || [];
    // The insertion point is the position of the running script. What it writes belongs
    // behind it, not at the end of the body. The point moves along with every node placed,
    // even across calls, so a script's second call lands behind the first instead of
    // directly behind the script again.
    var scriptNid = globalThis.__currentScriptNid || 0;
    var after = null;
    if (scriptNid) {
      var anchorNid = this._writeAnchorScript === scriptNid && this._writeAnchorNid
        ? this._writeAnchorNid
        : scriptNid;
      var anchor = _wrap(anchorNid);
      if (anchor && anchor.parentNode) after = anchor;
    }
    for (var i = 0; i < placements.length; i++) {
      var parentNid = +placements[i][0];
      var node = _wrap(+placements[i][1]);
      if (!node) continue;
      if (parentNid) {
        var parent = _wrap(parentNid);
        if (parent) parent.appendChild(node);
        continue;
      }
      if (after) {
        after.parentNode.insertBefore(node, after.nextSibling);
        after = node;
      } else {
        body.appendChild(node);
      }
    }
    if (scriptNid && after) {
      this._writeAnchorScript = scriptNid;
      this._writeAnchorNid = after._nid;
    }
  }
  writeln(...args) {
    this.write(args.join('') + '\n');
  }
  open() {
    var body = this.body;
    if (body) body.innerHTML = '';
    // A new parse begins. Whatever the input stream still held is gone.
    _dom("document_write_reset");
    this._writeAnchorScript = 0;
    this._writeAnchorNid = 0;
    return this;
  }
  close() {
    return;
  }
  hasFocus() { return true; }
  execCommand() { return false; }
}

class DocumentFragment extends Node {
  // `new DocumentFragment()` is legal in real browsers (creates a fresh
  // detached fragment). Bot detectors' domrect fixtures use it; without this
  // fallback `_nid` stayed undefined and the first appendChild tripped the
  // _dom Illegal-invocation guard — an escaping error the detector treats as
  // a broken environment.
  constructor(nid) {
    if (nid === undefined || nid === null || isNaN(+nid)) {
      nid = +_dom("create_document_fragment");
    }
    super(nid);
  }
  get nodeType() { return 11; }
  get nodeName() { return "#document-fragment"; }
  get innerHTML() { return _domParse("inner_html", this._nid) ?? ""; }
  set innerHTML(v) { _dom("set_inner_html", this._nid, String(v ?? "")); }
  querySelector(s) { return _wrapEl(+_dom("query_selector_scoped", this._nid, s)); }
  querySelectorAll(s) {
    const ids = _domParse("query_selector_all_scoped", this._nid, s) || [];
    return _nodeList(ids.map(_wrapEl).filter(Boolean));
  }
  get children() {
    const ids = _domParse("element_children", this._nid) || [];
    return HTMLCollection._from(ids.map(_wrapEl).filter(Boolean));
  }
  get firstElementChild() { return this.children[0] || null; }
  get lastElementChild() { const ch = this.children; return ch[ch.length - 1] || null; }
  getElementById(id) {
    const needle = String(id);
    const stack = Array.from(this.childNodes || []).reverse();
    while (stack.length) {
      const node = stack.pop();
      if (!node) continue;
      if (node.nodeType === 1 && node.id === needle) return node;
      const children = node.childNodes || [];
      for (let i = children.length - 1; i >= 0; i--) stack.push(children[i]);
    }
    return null;
  }
  cloneNode(deep) {
    const frag = document.createDocumentFragment();
    if (deep) frag.innerHTML = this.innerHTML;
    return frag;
  }
}

class DocumentType extends Node {
  constructor(nid, name, publicId, systemId) {
    super(nid);
    this._name = name;
    this._publicId = publicId;
    this._systemId = systemId;
  }
  get nodeType() { return 10; }
  get nodeName() { return this._name; }
  get name() { return this._name; }
  get publicId() { return this._publicId; }
  get systemId() { return this._systemId; }
  get nodeValue() { return null; }
  set nodeValue(v) {}
  get ownerDocument() { return this._ownerDocument || globalThis.document; }
}

const _cache = new Map();
function _elementClassFor(nid) {
  const tag = _domParse("tag_name", nid);
  if (tag === "FORM" && globalThis.HTMLFormElement) return globalThis.HTMLFormElement;
  return Element;
}
function _wrap(nid) {
  if (nid < 0 || nid === null || nid === undefined || isNaN(nid)) return null;
  if (_cache.has(nid)) return _cache.get(nid);
  const t = +_dom("node_type", nid);
  let n;
  if (t === 1) { const C = _elementClassFor(nid); n = new C(nid); }
  else if (t === 3) n = new Text(nid);
  else if (t === 8) n = new Comment(nid);
  else if (t === 9) n = new Document(nid);
  else n = new Node(nid);
  _cache.set(nid, n);
  return n;
}
function _wrapEl(nid) {
  if (nid < 0 || nid === null || nid === undefined || isNaN(nid)) return null;
  if (_cache.has(nid)) return _cache.get(nid);
  const C = _elementClassFor(nid);
  const n = new C(nid);
  _cache.set(nid, n);
  return n;
}

globalThis._wrap = _wrap;
globalThis.self = globalThis;

// `document` is a lazy accessor: at snapshot/bootstrap time there is no DOM
// yet (ObscuraState.dom is None until Page::init_js calls set_dom after the
// runtime constructor ran __diting_init), so the document node id cannot be
// resolved eagerly. Resolving on first access also guarantees identity: the
// global document IS `_wrap(documentNid)` — the same cached instance that
// parentNode bubbling reaches. A separate `new Document(...)` here meant
// React's delegated listeners (attached to the global `document`) lived on a
// different object than the bubble target, so submit/click handlers never ran.
let _documentInstance = null;
Object.defineProperty(globalThis, 'document', {
  configurable: true,
  enumerable: true,
  get() {
    if (_documentInstance) return _documentInstance;
    try {
      const nid = +_dom("document_node_id"); // NaN while no DOM is attached
      if (isNaN(nid) || nid < 0) return null;
      _documentInstance = _wrap(nid);
    } catch (e) {
      return null; // ops unavailable during snapshot construction
    }
    return _documentInstance;
  },
  set(v) { _documentInstance = v; },
});
function _resolveUrl(url) {
  // Coerce up front: a URL object passed to location.href/assign/replace has
  // no .startsWith and would throw here (upstream fe26417).
  url = String(url);
  if (!url) return url;
  if (url.startsWith('http://') || url.startsWith('https://') || url.startsWith('about:')) return url;
  try { return new URL(url, _docBase()).href; } catch(e) { return url; }
}
// `__virtualUrl` is set by `history.pushState`/`replaceState` (and cleared by
// any real navigation). When set, `location.href` and friends read it instead
// of the underlying `document_url`. Without this, client-side routers
// (Next.js, React Router, vue-router) call `pushState` but the URL never
// changes, so their `useLocation` hooks return the wrong path and the UI
// freezes on the original route.
globalThis.__virtualUrl = null;
function __currentUrl() {
  return globalThis.__virtualUrl || _domParse("document_url") || "about:blank";
}
globalThis.location = {
  get href() { return __currentUrl(); },
  set href(url) { var r = _resolveUrl(url); globalThis.__virtualUrl = r; _OPS.op_navigate(r, 'GET', ''); },
  get origin() { try { return new URL(this.href).origin; } catch { return ""; } },
  get protocol() { try { return new URL(this.href).protocol; } catch { return ""; } },
  get host() { try { return new URL(this.href).host; } catch { return ""; } },
  get hostname() { try { return new URL(this.href).hostname; } catch { return ""; } },
  get pathname() { try { return new URL(this.href).pathname; } catch { return "/"; } },
  get search() { try { return new URL(this.href).search; } catch { return ""; } },
  get hash() { try { return new URL(this.href).hash; } catch { return ""; } },
  get port() { try { return new URL(this.href).port; } catch { return ""; } },
  toString() { return this.href; },
  assign(url) { var r = _resolveUrl(url); globalThis.__virtualUrl = r; _OPS.op_navigate(r, 'GET', ''); },
  reload() { var r = _resolveUrl(this.href); globalThis.__virtualUrl = r; _OPS.op_navigate(r, 'GET', ''); },
  replace(url) { var r = _resolveUrl(url); globalThis.__virtualUrl = r; _OPS.op_navigate(r, 'GET', ''); },
};
const _locationObj = globalThis.location;
Object.defineProperty(globalThis, 'location', {
  get() { return _locationObj; },
  set(url) { var r = _resolveUrl(String(url)); globalThis.__virtualUrl = r; _OPS.op_navigate(r, 'GET', ''); },
  configurable: false,
  enumerable: true,
});

globalThis.window = globalThis;
globalThis.self = globalThis;
globalThis.top = globalThis;
globalThis.parent = globalThis;
globalThis.frames = globalThis;
globalThis.frameElement = null;
globalThis.length = 0;

// HTML spec exposes on* event handler IDL attributes on Window. Libraries like
// jQuery feature-detect bubbling via `("on" + ev) in window` and fall back to
// a legacy IE path that crashes on missing DOM APIs when the check returns
// false. Initialising them to null makes the check match real browsers.
for (const _ev of [
  "abort","beforeprint","beforeunload","blur","cancel","canplay","canplaythrough",
  "change","click","close","contextmenu","cuechange","dblclick","drag","dragend",
  "dragenter","dragleave","dragover","dragstart","drop","durationchange","emptied",
  "ended","error","focus","focusin","focusout","formdata","gotpointercapture",
  "hashchange","input","invalid","keydown","keypress","keyup","languagechange",
  "load","loadeddata","loadedmetadata","loadstart","lostpointercapture","message",
  "mousedown","mouseenter","mouseleave","mousemove","mouseout","mouseover","mouseup",
  "offline","online","pagehide","pageshow","paste","pause","play","playing",
  "pointercancel","pointerdown","pointerenter","pointerleave","pointermove",
  "pointerout","pointerover","pointerup","popstate","progress","ratechange",
  "rejectionhandled","reset","resize","scroll","seeked","seeking","select",
  "stalled","storage","submit","suspend","timeupdate","toggle","unhandledrejection",
  "unload","volumechange","waiting","wheel",
]) {
  if (!(("on" + _ev) in globalThis)) globalThis["on" + _ev] = null;
}

globalThis.Window = globalThis.Window || function Window() {};
Object.defineProperty(globalThis.Window, Symbol.hasInstance, {
  value(obj) { return obj === globalThis || (obj && obj.window === obj); },
  configurable: true,
});
// Framework environment gates (Ember's among them) check the direct identity
// `self.constructor === Window`; the inherited Object constructor sends them
// down their server-rendering path (upstream 9dfc67a).
Object.defineProperty(globalThis, 'constructor', {
  value: globalThis.Window,
  writable: true,
  configurable: true,
  enumerable: false,
});


const _iframeRegistry = [];
function _registerIframe(iframeEl) {
  // Idempotent: attachment points (appendChild/innerHTML) and src loading can
  // both reach the same element.
  if (iframeEl._iframeRegistered) return;
  iframeEl._iframeRegistered = true;
  const idx = _iframeRegistry.length;
  _iframeRegistry.push(iframeEl);
  globalThis.length = _iframeRegistry.length;
  Object.defineProperty(globalThis, idx, {
    get() {
      // Eagerly create the browsing context: detector code reads `self[i]`
      // right after inserting fixture markup, before ever touching
      // contentWindow — a null there reads as a missing iframe.
      if (!iframeEl._iframeWin && typeof iframeEl.contentWindow !== 'undefined') {
        void iframeEl.contentWindow;
      }
      return iframeEl._iframeWin || null;
    },
    configurable: true,
    enumerable: false,
  });
}
// Browsers create an iframe's browsing context when it is INSERTED INTO the
// document (not on src load, not on contentWindow access). Detector code
// captures `e = self.length`, appends fixture markup containing an iframe,
// then reads `self[e]` to get that iframe's window. Register iframes found in
// an inserted subtree, in tree order, so numeric window indexing matches.
function _registerIframesIn(node) {
  if (!node || typeof node.querySelectorAll !== 'function') return;
  const frames = node.nodeType === 1 && node.tagName === 'IFRAME'
    ? [node, ...node.querySelectorAll('iframe')]
    : [...node.querySelectorAll('iframe')];
  for (const f of frames) _registerIframe(f);
}
function _nodeInDocument(node) {
  let cur = node;
  let steps = 0;
  while (cur && steps++ < 200) {
    if (cur.nodeType === 9) return true;
    cur = cur.parentNode;
  }
  return false;
}
// Derive navigator.platform / userAgentData.platform from the UA so the JS
// layer's claimed OS matches the HTTP-layer UA. Mismatches (macOS UA +
// "Linux" platform) are a strong anti-bot signal (Baidu Wenku 安全验证).
function __ditingPlatformFromUA() {
  const ua = globalThis.__diting_ua || "";
  if (ua.indexOf("Windows") !== -1) return "Windows";
  if (ua.indexOf("Macintosh") !== -1 || ua.indexOf("Mac OS X") !== -1) return "MacIntel";
  if (ua.indexOf("iPhone") !== -1 || ua.indexOf("iPad") !== -1) return "iPhone";
  if (ua.indexOf("Android") !== -1) return "Linux armv8l";
  return "Linux x86_64";
}
function __ditingUADataPlatformFromUA() {
  const ua = globalThis.__diting_ua || "";
  if (ua.indexOf("Windows") !== -1) return "Windows";
  if (ua.indexOf("Macintosh") !== -1 || ua.indexOf("Mac OS X") !== -1) return "macOS";
  if (ua.indexOf("Android") !== -1) return "Android";
  return "Linux";
}

// ---- Font-presence metrics -------------------------------------------------
// Font-probe fingerprinters (Castle's fonts collector, fingerprintjs,
// WorkOS Radar) append a hidden 72px span and compare offsetWidth/offsetHeight
// across `'Font Name', serif/sans-serif/monospace` lists: an INSTALLED font
// changes the text box, a missing one falls back to the generic family and
// matches the baseline exactly. The installed sets below are the fonts a real
// Chrome can resolve per OS — Windows-only faces (Segoe UI, Calibri,
// Consolas) are absent on macOS and vice versa (Menlo, Monaco), so the
// detected set stays coherent with the UA persona.
const _OBSCURA_FONT_PLATFORM = () => {
  const p = __ditingPlatformFromUA();
  if (p === "Windows") return "win";
  if (p === "MacIntel" || p === "iPhone") return "mac";
  return "linux";
};
const _INSTALLED_FONTS = {
  mac: ["Arial", "Verdana", "Helvetica", "Tahoma", "Trebuchet MS", "Georgia",
    "Courier New", "Brush Script MT", "Comic Sans MS", "Impact", "Menlo", "Monaco"],
  win: ["Arial", "Verdana", "Tahoma", "Trebuchet MS", "Georgia", "Garamond",
    "Courier New", "Brush Script MT", "Palatino Linotype", "Lucida Console",
    "Comic Sans MS", "Impact", "Lucida Sans Unicode", "Century Gothic",
    "Segoe UI", "Cambria", "Calibri", "Consolas"],
  linux: ["Arial", "Courier New", "DejaVu Sans", "Ubuntu", "Verdana",
    "Trebuchet MS", "Times New Roman", "Georgia", "Cantarell"],
};
// Stable per-name width factor for an installed font, or null when the OS
// persona doesn't have it. Always ≥1% off the generic baseline (0.92–1.14,
// values inside ±1% nudged out) so the probe reliably detects installed
// faces, exactly like a real font's metrics never coincide with the fallback.
function _installedFontFactor(name) {
  const list = _INSTALLED_FONTS[_OBSCURA_FONT_PLATFORM()] || _INSTALLED_FONTS.mac;
  if (list.indexOf(name) === -1) return null;
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (Math.imul(h, 31) + name.charCodeAt(i)) | 0;
  let f = 0.92 + ((h >>> 0) % 23) / 100;
  if (f > 0.99 && f < 1.01) f += 0.03;
  return f;
}
// Per-character advance widths (em) approximating Helvetica for lowercase,
// with caps/digits from the same metrics table. Enough spread for
// "mmMwWLli10Oo#@"-style probe strings to discriminate families.
const _FONT_CHAR_EM = (ch) => {
  switch (ch) {
    case "i": case "l": case "j": return 0.222;
    case "t": case "f": return 0.278;
    case "r": return 0.333;
    case "m": return 0.833;
    case "w": return 0.722;
    case "M": case "W": return 0.944;
    case "L": return 0.611;
    case "O": case "Q": case "G": case "D": case "U": case "H": case "B": case "N": case "R": case "S": case "E": case "K": return 0.722;
    case "C": case "P": case "F": case "A": case "V": case "X": case "Y": return 0.667;
    case "T": case "J": case "Z": case "I": return 0.611;
    case "o": case "0": case "1": case "#": case "e": case "a": case "s": case "c": case "u": case "n": case "d": case "g": case "p": case "q": case "b": case "h": return 0.556;
    case "@": return 0.833;
    default: return /[A-Z]/.test(ch) ? 0.7 : 0.5;
  }
};
// Generic-family width scale + line-height factor (em multiples).
const _GENERIC_FONT = {
  "serif": { w: 0.95, h: 1.15 },
  "sans-serif": { w: 1.0, h: 1.2 },
  "monospace": { w: 1.18, h: 1.21 },
  "cursive": { w: 1.02, h: 1.2 },
  "fantasy": { w: 1.04, h: 1.2 },
  "system-ui": { w: 1.0, h: 1.2 },
  "ui-sans-serif": { w: 1.0, h: 1.2 },
};
// Synthesize a leaf text element's content box from its inline font styles.
// Returns null for elements we shouldn't size this way (containers, no text)
// so their callers keep the default synthesized cell.
function _obscuraFontBox(el) {
  try {
    const st = el.style;
    if (!st || !st._props) return null;
    const props = st._props;
    const display = props["display"];
    if (display === "none") return { w: 0, h: 0 };
    if (el.children && el.children.length) return null;
    const text = (el.textContent || "").trim();
    if (!text) return null;
    const fontSize = parseFloat(props["font-size"] || props["fontSize"]) || 16;
    const famRaw = props["font-family"] || props["fontFamily"] || "serif";
    const names = String(famRaw).split(",").map((s) => s.trim().replace(/^['"]|['"]$/g, "")).filter(Boolean);
    // First installed family wins; otherwise the trailing generic.
    let chosen = null, generic = null;
    for (const n of names) {
      if (_GENERIC_FONT[n]) { generic = generic || n; continue; }
      if (chosen === null && _installedFontFactor(n) !== null) { chosen = n; break; }
    }
    if (!generic) generic = /sans/i.test(famRaw) ? "sans-serif" : "serif";
    const g = _GENERIC_FONT[generic] || _GENERIC_FONT["serif"];
    const ff = chosen ? _installedFontFactor(chosen) : 1;
    let w = 0;
    for (const ch of text) w += _FONT_CHAR_EM(ch);
    return {
      w: Math.round(w * fontSize * g.w * ff),
      h: Math.round(fontSize * g.h * (chosen ? 1 + (ff - 1) * 0.45 : 1)),
    };
  } catch { return null; }
}

// PluginArray / MimeTypeArray / Plugin / MimeType — real browsers expose these
// constructors globally, and bot detectors reference them directly (absent →
// ReferenceError) or check `navigator.plugins instanceof PluginArray`.
// Making them real classes (not plain arrays) keeps instanceof working.
globalThis.Plugin = class Plugin {
  constructor(name, filename, description) { this.name = name; this.filename = filename; this.description = description; this.length = 1; }
};
globalThis.MimeType = class MimeType {
  constructor(type, description, suffixes, enabledPlugin) { this.type = type; this.description = description; this.suffixes = suffixes; this.enabledPlugin = enabledPlugin; }
};
globalThis.PluginArray = class PluginArray extends Array {
  item(i) { return this[i] || null; }
  namedItem(name) { return this.find(p => p.name === name) || null; }
  refresh() {}
};
globalThis.MimeTypeArray = class MimeTypeArray extends Array {
  item(i) { return this[i] || null; }
  namedItem(name) { return this.find(m => m.type === name) || null; }
};
// Cached singletons: real browsers return the same instance on every access
// (`navigator.plugins === navigator.plugins`); fresh instances would be a
// fingerprint anomaly.
let _pluginsInst = null, _mimeTypesInst = null;

// navigator.connection must be an EventTarget-shaped object: analytics and
// adaptive-streaming libraries register 'change' listeners on it (upstream
// fc9f524 — the old data-only object lacked addEventListener entirely, so
// `navigator.connection.addEventListener` threw). dispatchEvent also runs
// the matching on* property handler, like a real EventTarget.
class NetworkInformation {
  constructor() { this._listeners = Object.create(null); }
  get downlink() { return 10; }
  get downlinkMax() { return Infinity; }
  get effectiveType() { return '4g'; }
  get rtt() { return 50; }
  get saveData() { return false; }
  get type() { return 'wifi'; }
  get onchange() { return this._onchange || null; }
  set onchange(v) { this._onchange = typeof v === "function" ? v : null; }
  get ontypechange() { return this._ontypechange || null; }
  set ontypechange(v) { this._ontypechange = typeof v === "function" ? v : null; }
  addEventListener(type, listener) {
    if (typeof listener !== "function") return;
    (this._listeners[type] || (this._listeners[type] = [])).push(listener);
  }
  removeEventListener(type, listener) {
    const listeners = this._listeners[type];
    if (listeners) this._listeners[type] = listeners.filter((item) => item !== listener);
  }
  dispatchEvent(event) {
    if (!event || !event.type) return true;
    for (const listener of this._listeners[event.type] || []) {
      try { listener.call(this, event); } catch (error) { console.error(error); }
    }
    const handler = this["on" + event.type];
    if (typeof handler === "function") {
      try { handler.call(this, event); } catch (error) { console.error(error); }
    }
    return !event.defaultPrevented;
  }
}
_markNative(NetworkInformation);

globalThis.navigator = {
  get userAgent() { return globalThis.__diting_ua || "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"; },
  get appVersion() { return this.userAgent.replace('Mozilla/', ''); },
  get platform() { return __ditingPlatformFromUA(); },
  get language() { return __ditingLangList()[0]; },
  get languages() { return __ditingLangList().slice(); },
  onLine: true, cookieEnabled: true, hardwareConcurrency: 8,
  maxTouchPoints: 0,
  vendor: "Google Inc.", product: "Gecko", productSub: "20030107",
  doNotTrack: null,
  deviceMemory: 8,
  connection: new NetworkInformation(),
  get webdriver() { return undefined; },
  pdfViewerEnabled: true,
  get plugins() {
    if (!_pluginsInst) {
      _pluginsInst = new PluginArray(
        new Plugin("PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
        new Plugin("Chrome PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
        new Plugin("Chromium PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
        new Plugin("Microsoft Edge PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
        new Plugin("WebKit built-in PDF", "internal-pdf-viewer", "Portable Document Format"),
      );
    }
    return _pluginsInst;
  },
  get mimeTypes() {
    if (!_mimeTypesInst) {
      _mimeTypesInst = new MimeTypeArray(
        new MimeType("application/pdf", "Portable Document Format", "pdf", null),
        new MimeType("text/pdf", "Portable Document Format", "pdf", null),
      );
    }
    return _mimeTypesInst;
  },
  userAgentData: {
    brands: [
      {brand: "Google Chrome", version: "145"},
      {brand: "Chromium", version: "145"},
      {brand: "Not=A?Brand", version: "24"},
    ],
    mobile: false,
    get platform() { return __ditingUADataPlatformFromUA(); },
    getHighEntropyValues(hints) {
      const plat = __ditingUADataPlatformFromUA();
      return Promise.resolve({
        architecture: "x86",
        bitness: "64",
        brands: [{brand:"Google Chrome",version:"145"},{brand:"Chromium",version:"145"},{brand:"Not=A?Brand",version:"24"}],
        fullVersionList: [{brand:"Google Chrome",version:"145.0.0.0"},{brand:"Chromium",version:"145.0.0.0"},{brand:"Not=A?Brand",version:"24.0.0.0"}],
        mobile: false,
        model: "",
        platform: plat,
        platformVersion: "15.2.0",
        uaFullVersion: "145.0.0.0",
      });
    },
    toJSON() { return {brands:this.brands,mobile:this.mobile,platform:this.platform}; },
  },
  serviceWorker: { ready: Promise.resolve(), register(){return Promise.resolve();}, getRegistrations(){return Promise.resolve([]);}, controller: null },
  mediaDevices: {
    enumerateDevices() {
      return Promise.resolve([
        {deviceId:"default",kind:"audioinput",label:"",groupId:"default"},
        {deviceId:"comms",kind:"audioinput",label:"",groupId:"comms"},
        {deviceId:"default",kind:"audiooutput",label:"",groupId:"default"},
        {deviceId:"",kind:"videoinput",label:"",groupId:""},
      ]);
    },
    getUserMedia() { return Promise.reject(new DOMException("NotAllowedError")); },
    getDisplayMedia() { return Promise.reject(new DOMException("NotAllowedError")); },
    addEventListener(){}, removeEventListener(){},
  },
  clipboard: { writeText(){return Promise.resolve();}, readText(){return Promise.resolve("");} },
  permissions: { query(params){
    if (params?.name === 'notifications') return Promise.resolve({state:"prompt",onchange:null});
    return Promise.resolve({state:"granted"});
  } },
  getBattery() { return Promise.resolve({ charging: _fp('batteryCharging'), chargingTime: _fp('batteryCharging') ? 0 : Infinity, dischargingTime: _fp('batteryCharging') ? Infinity : Math.floor(3600 + _fpRand(250) * 7200), level: _fp('batteryLevel'), addEventListener(){} }); },
  getGamepads() { return []; },
  sendBeacon() { return true; },
  javaEnabled() { return false; },
  geolocation: {
    getCurrentPosition(success, error) {
      const coords = {
        latitude: 50.1109 + (_fpRand(500) - 0.5) * 0.1,
        longitude: 8.6821 + (_fpRand(501) - 0.5) * 0.1,
        accuracy: 10 + _fpRand(502) * 40,
        altitude: null,
        altitudeAccuracy: null,
        heading: null,
        speed: null,
      };
      const pos = { coords, timestamp: Date.now() };
      if (typeof success === 'function') success(pos);
    },
    watchPosition(success, error) {
      if (typeof success === 'function') {
        const coords = {
          latitude: 50.1109 + (_fpRand(503) - 0.5) * 0.1,
          longitude: 8.6821 + (_fpRand(504) - 0.5) * 0.1,
          accuracy: 10 + _fpRand(505) * 40,
          altitude: null,
          altitudeAccuracy: null,
          heading: null,
          speed: null,
        };
        success({ coords, timestamp: Date.now() });
      }
      return 0;
    },
    clearWatch() {},
  },
  storage: {
    estimate() { return Promise.resolve({ quota: 5000000000, usage: Math.floor(_fpRand(640) * 100000000) }); },
    persist() { return Promise.resolve(false); },
    persisted() { return Promise.resolve(false); },
  },
};

globalThis.chrome = {
  app: { isInstalled: false, InstallState: { DISABLED: "disabled", INSTALLED: "installed", NOT_INSTALLED: "not_installed" }, RunningState: { CANNOT_RUN: "cannot_run", READY_TO_RUN: "ready_to_run", RUNNING: "running" } },
  runtime: { OnInstalledReason: {}, OnRestartRequiredReason: {}, PlatformArch: {}, PlatformNaclArch: {}, PlatformOs: {}, RequestUpdateCheckStatus: {}, connect() { return {}; }, sendMessage() {} },
  csi() {
    const t = Date.now();
    return { onloadT: t, startE: t - Math.floor(100 + _fpRand(610) * 200), pageT: 0, tran: 5, flashVersion: "" };
  },
  loadTimes() {
    const t = Date.now() / 1000;
    const request = t - 0.5 - _fpRand(611) * 0.5;
    const startLoad = request + 0.05 + _fpRand(612) * 0.02;
    const commit = request + 0.3 + _fpRand(613) * 0.4;
    const finishDoc = commit + 0.1 + _fpRand(614) * 0.2;
    const finish = finishDoc + 0.05 + _fpRand(615) * 0.1;
    const firstPaint = commit + 0.03 + _fpRand(616) * 0.1;
    const navTypes = ["BackForward","Reload","Link","Other"];
    return {
      requestTime: request, startLoadTime: startLoad * 1000, commitLoadTime: commit * 1000,
      finishDocumentLoadTime: finishDoc * 1000, finishLoadTime: finish * 1000,
      firstPaintTime: firstPaint * 1000, firstPaintAfterLoadTime: 0,
      navigationType: navTypes[Math.floor(_fpRand(617) * 4)],
      wasFetchedViaSpdy: false, wasNpnNegotiated: false,
      npnNegotiatedProtocol: "http/1.1",
      wasAlternateProtocolAvailable: false, connectionInfo: "http/1.1",
    };
  },
};

globalThis.Notification = class Notification {
  static permission = "default";
  static requestPermission() { return Promise.resolve(Notification.permission); }
  constructor() {}
};

globalThis.WebGLRenderingContext = class WebGLRenderingContext {};
globalThis.WebGL2RenderingContext = class WebGL2RenderingContext {};
// Bot detectors verify these methods exist on the PROTOTYPE (they grab
// `WebGLRenderingContext.prototype.getParameter` etc. to test for API
// tampering); the per-canvas instance methods alone are not enough.
// These prototype versions don't touch `this`, so they also survive being
// called with a fake receiver.
// Shared getParameter implementation with realistic ANGLE values. Array-valued
// pnames must return iterables — detectors spread them (`[...gl.getParameter(gl.MAX_VIEWPORT_DIMS)]`).
function _glParam(pname) {
  switch (pname) {
    case 0x9245: return _fp('gpuVendor');
    case 0x9246: return _fp('gpu');
    case 0x1F01: return 'WebKit WebGL';
    case 0x1F00: return 'WebKit';
    case 0x1F02: return 'OpenGL ES 3.0 (ANGLE)';
    case 0x8B8C: return 'WebGL GLSL ES 3.00 (ANGLE)';
    case 0x0D3A: return new Int32Array([32767, 32767]);   // MAX_VIEWPORT_DIMS
    case 0x846E: return new Float32Array([1, 10]);        // ALIASED_LINE_WIDTH_RANGE
    case 0x846D: return new Float32Array([1, 1024]);      // ALIASED_POINT_SIZE_RANGE
    case 0x0B70: return new Float32Array([0, 1]);         // DEPTH_RANGE
    case 0x0C22: return new Float32Array([0, 0, 0, 0]);   // COLOR_CLEAR_VALUE
    case 0x0C2D: return new Float32Array([0, 0, 0, 0]);   // BLEND_COLOR
    case 0x86A3: return new Uint32Array(0);               // COMPRESSED_TEXTURE_FORMATS
    case 0x0D33: return 16384;   // MAX_TEXTURE_SIZE
    case 0x0D38: return 16384;   // MAX_CUBE_MAP_TEXTURE_SIZE
    case 0x84E8: return 16384;   // MAX_RENDERBUFFER_SIZE
    case 0x8869: return 16;      // MAX_VERTEX_ATTRIBS
    case 0x8872: return 16;      // MAX_TEXTURE_IMAGE_UNITS
    case 0x8B4C: return 16;      // MAX_VERTEX_TEXTURE_IMAGE_UNITS
    case 0x8B4D: return 32;      // MAX_COMBINED_TEXTURE_IMAGE_UNITS
    case 0x8DFB: return 4096;    // MAX_VERTEX_UNIFORM_VECTORS
    case 0x8DFD: return 1024;    // MAX_FRAGMENT_UNIFORM_VECTORS
    case 0x8DFC: return 30;      // MAX_VARYING_VECTORS
    case 0x84FF: return 16;      // MAX_TEXTURE_MAX_ANISOTROPY_EXT
    case 0x8513: return 4;       // SAMPLES
    case 0x80A9: return 1;       // SAMPLE_BUFFERS
    case 0x0D50: return 4;       // SUBPIXEL_BITS
    case 0x0D52: case 0x0D53: case 0x0D54: case 0x0D55: return 8;  // R/G/B/A BITS
    case 0x0D56: return 24;      // DEPTH_BITS
    case 0x0D57: return 0;       // STENCIL_BITS
    default: return 0;
  }
}
// GL constants. Real browsers expose these on the interface PROTOTYPE (WebIDL
// constant semantics), so instances resolve them through the chain — including
// our Proxy-backed contexts via `prop in target`. Without them, `gl.MAX_VIEWPORT_DIMS`
// returned our numNoop sentinel instead of 0x0D3A and getParameter got garbage.
const _GL_CONSTS = {
  MAX_VIEWPORT_DIMS: 0x0D3A, ALIASED_LINE_WIDTH_RANGE: 0x846E, ALIASED_POINT_SIZE_RANGE: 0x846D,
  DEPTH_RANGE: 0x0B70, COLOR_CLEAR_VALUE: 0x0C22, BLEND_COLOR: 0x0C2D, COMPRESSED_TEXTURE_FORMATS: 0x86A3,
  MAX_TEXTURE_SIZE: 0x0D33, MAX_CUBE_MAP_TEXTURE_SIZE: 0x0D38, MAX_RENDERBUFFER_SIZE: 0x84E8,
  MAX_VERTEX_ATTRIBS: 0x8869, MAX_TEXTURE_IMAGE_UNITS: 0x8872, MAX_VERTEX_TEXTURE_IMAGE_UNITS: 0x8B4C,
  MAX_COMBINED_TEXTURE_IMAGE_UNITS: 0x8B4D, MAX_VERTEX_UNIFORM_VECTORS: 0x8DFB,
  MAX_FRAGMENT_UNIFORM_VECTORS: 0x8DFD, MAX_VARYING_VECTORS: 0x8DFC, MAX_TEXTURE_MAX_ANISOTROPY_EXT: 0x84FF,
  SAMPLES: 0x8513, SAMPLE_BUFFERS: 0x80A9, SUBPIXEL_BITS: 0x0D50,
  RED_BITS: 0x0D52, GREEN_BITS: 0x0D53, BLUE_BITS: 0x0D54, ALPHA_BITS: 0x0D55,
  DEPTH_BITS: 0x0D56, STENCIL_BITS: 0x0D57,
};
const _GL_EXT_ANISO = { MAX_TEXTURE_MAX_ANISOTROPY_EXT: 0x84FF };
const _GL_EXT_DEBUG = { UNMASKED_VENDOR_WEBGL: 0x9245, UNMASKED_RENDERER_WEBGL: 0x9246 };
const _GL_EXT_LOSE = { loseContext() {}, restoreContext() {} };
for (const _GLC of [globalThis.WebGLRenderingContext, globalThis.WebGL2RenderingContext]) {
  Object.assign(_GLC.prototype, _GL_CONSTS);
  _GLC.prototype.getParameter = function(pname) { return _glParam(pname); };
  // getExtension must return live objects for the extensions we advertise —
  // detectors chain-read constants off the result
  // (`gl.getExtension('EXT_texture_filter_anisotropic').MAX_TEXTURE_MAX_ANISOTROPY_EXT`).
  _GLC.prototype.getExtension = function(name) {
    if (name === 'WEBGL_debug_renderer_info') return _GL_EXT_DEBUG;
    if (name === 'EXT_texture_filter_anisotropic') return _GL_EXT_ANISO;
    if (name === 'WEBGL_lose_context') return _GL_EXT_LOSE;
    return null;
  };
  _GLC.prototype.getSupportedExtensions = function() { return ['WEBGL_debug_renderer_info','EXT_texture_filter_anisotropic','WEBGL_compressed_texture_s3tc','WEBGL_lose_context']; };
  _GLC.prototype.getShaderPrecisionFormat = function() { return { rangeMin: 127, rangeMax: 127, precision: 23 }; };
  _GLC.prototype.bufferData = function() {};
  _GLC.prototype.readPixels = function(x,y,w,h,f,t,d) { if(d) for(let i=0;i<d.length;i++) d[i]=0; };
}

globalThis.screen = { width:1920, height:1080, availWidth:1920, availHeight:1040, colorDepth:24, pixelDepth:24, availTop:0, availLeft:0, orientation:{type:"landscape-primary",angle:0,addEventListener(){},removeEventListener(){},dispatchEvent(){return true;}} };
globalThis.visualViewport = { width:1920, height:1000, offsetLeft:0, offsetTop:0, scale:1, addEventListener(){}, removeEventListener(){} };
globalThis.devicePixelRatio = 1;
globalThis.innerWidth = 1920; globalThis.innerHeight = 1000;
globalThis.outerWidth = 1920; globalThis.outerHeight = 1080;
globalThis.scrollX = 0; globalThis.scrollY = 0;
globalThis.pageXOffset = 0; globalThis.pageYOffset = 0;

globalThis.__fetchInterceptEnabled = false;
globalThis.__fetchInterceptCallback = null; // Set by CDP to handle paused requests

function _base64ToUint8Array(b64) {
  const clean = String(b64 || '').replace(/[\r\n\s]/g, '');
  if (!clean) return new Uint8Array();
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const padding = clean.endsWith('==') ? 2 : (clean.endsWith('=') ? 1 : 0);
  const bytes = new Uint8Array((clean.length * 3 >> 2) - padding);
  let out = 0;
  for (let i = 0; i < clean.length; i += 4) {
    const a = alphabet.indexOf(clean[i]);
    const b = alphabet.indexOf(clean[i + 1]);
    const c = clean[i + 2] === '=' ? 0 : alphabet.indexOf(clean[i + 2]);
    const d = clean[i + 3] === '=' ? 0 : alphabet.indexOf(clean[i + 3]);
    const n = (a << 18) | (b << 12) | (c << 6) | d;
    if (out < bytes.length) bytes[out++] = (n >> 16) & 0xff;
    if (out < bytes.length) bytes[out++] = (n >> 8) & 0xff;
    if (out < bytes.length) bytes[out++] = n & 0xff;
  }
  return bytes;
}

function _bodyToUint8Array(body) {
  if (body == null) return new Uint8Array();
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  // obscura's Blob materializes its data into _bytes in the constructor.
  if (body._bytes instanceof Uint8Array) return body._bytes;
  return new TextEncoder().encode(String(body));
}

// Latin-1 binary string: one char per byte (kept for callers that need the
// legacy form).
function _bytesToBinaryString(bytes) { let s = ""; for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]); return s; }

// Base64 of raw bytes — the wire form for request bodies. The old Latin-1
// binary string corrupted non-UTF-8 byte sequences on the deno #[string]
// boundary ([0,128,255] became [0,194,128,195,191], upstream obscura #716);
// base64 is byte-exact across that boundary.
function _bytesToBase64(bytes) {
  const c="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let r="";
  for(let i=0;i<bytes.length;i+=3){
    const a=bytes[i],b=bytes[i+1],d=bytes[i+2];
    r+=c[a>>2]+c[((a&3)<<4)|((b??0)>>4)]+(i+1<bytes.length?c[((b&15)<<2)|((d??0)>>6)]:"=")+(i+2<bytes.length?c[d&63]:"=");
  }
  return r;
}

function _concatBytes(arrays) {
  let total = 0;
  for (let i = 0; i < arrays.length; i++) total += arrays[i].length;
  const out = new Uint8Array(total);
  let off = 0;
  for (let i = 0; i < arrays.length; i++) { out.set(arrays[i], off); off += arrays[i].length; }
  return out;
}

function _arrayBufferFromBytes(bytes) {
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
}

function _installWasmStreamingFallback() {
  if (typeof WebAssembly === 'undefined') return;
  if (WebAssembly.instantiateStreaming && WebAssembly.instantiateStreaming.__ditingFallback) return;
  const nativeInstantiateStreaming = WebAssembly.instantiateStreaming;
  const fallback = async function instantiateStreaming(source, imports) {
    const response = await source;
    if (response && typeof response.arrayBuffer === 'function') {
      return WebAssembly.instantiate(await response.arrayBuffer(), imports);
    }
    if (typeof nativeInstantiateStreaming === 'function') {
      return nativeInstantiateStreaming.call(WebAssembly, response, imports);
    }
    return WebAssembly.instantiate(response, imports);
  };
  fallback.__ditingFallback = true;
  WebAssembly.instantiateStreaming = fallback;
}
_installWasmStreamingFallback();

globalThis.fetch = async (input, init = {}) => {
  let url = typeof input === "string"
    ? input
    : (input instanceof Request
      ? input.url
      : ((typeof URL === 'function' && input instanceof URL) ? input.href : (input?.url || input?.href || String(input || ""))));
  if (url && !url.includes('://')) {
    try {
      const base = _docBase();
      url = new URL(url, base).href;
    } catch(e) { /* keep as-is if URL resolution fails */ }
  }
  const method = init.method || (input instanceof Request ? input.method : "GET");
  const hdrObj = init.headers instanceof Headers ? Object.fromEntries(init.headers.entries()) : Object.assign({}, init.headers || {});
  // fetch(request) with no explicit init.body inherits the Request's body
  // (upstream #716 split, item: "A Request object's inherited body is also
  // dropped when fetch(request) has no explicit init.body").
  if (init.body == null && input instanceof Request && input.body != null) {
    init = { ...init, body: input.body };
  }
  let body = "";
  let bodyIsBase64 = false;
  if (init.body != null) {
    if (typeof FormData === "function" && init.body instanceof FormData) {
      // multipart/form-data with a generated boundary, like a real browser.
      const boundary = "----ditingFormBoundary" + Math.random().toString(16).slice(2) + Date.now().toString(16);
      const chunks = [];
      const str = (s) => new TextEncoder().encode(s);
      for (const [k, v] of init.body.entries()) {
        const name = String(k).replace(/"/g, "%22");
        // Blob/File values become a filename part with their own Content-Type
        // (upstream 3eb28da); plain String(v) would send "[object Blob]".
        if (v != null && typeof v === "object" && v._bytes instanceof Uint8Array) {
          chunks.push(str("--" + boundary + "\r\nContent-Disposition: form-data; name=\"" + name + "\"; filename=\"" + String(v.name || "blob").replace(/"/g, "%22") + "\"\r\nContent-Type: " + (v.type || "application/octet-stream") + "\r\n\r\n"));
          chunks.push(v._bytes);
          chunks.push(str("\r\n"));
        } else {
          chunks.push(str("--" + boundary + "\r\nContent-Disposition: form-data; name=\"" + name + "\"\r\n\r\n" + String(v) + "\r\n"));
        }
      }
      chunks.push(str("--" + boundary + "--\r\n"));
      body = _bytesToBase64(_concatBytes(chunks));
      bodyIsBase64 = true;
      if (!Object.keys(hdrObj).some(h => h.toLowerCase() === "content-type")) {
        hdrObj["content-type"] = "multipart/form-data; boundary=" + boundary;
      }
    } else if (typeof URLSearchParams === "function" && init.body instanceof URLSearchParams) {
      body = init.body.toString();
      if (!Object.keys(hdrObj).some(h => h.toLowerCase() === "content-type")) {
        hdrObj["content-type"] = "application/x-www-form-urlencoded;charset=UTF-8";
      }
    } else if (typeof Blob === "function" && init.body instanceof Blob) {
      // Blob posts its bytes with its type (upstream 260c4c0).
      if (init.body.type && !Object.keys(hdrObj).some(h => h.toLowerCase() === "content-type")) {
        hdrObj["content-type"] = init.body.type;
      }
      body = _bytesToBase64(init.body._bytes);
      bodyIsBase64 = true;
    } else if (init.body instanceof ArrayBuffer || ArrayBuffer.isView(init.body)) {
      body = _bytesToBase64(_bodyToUint8Array(init.body));
      bodyIsBase64 = true;
    } else {
      body = String(init.body);
    }
  }
  if (bodyIsBase64 && body.length) hdrObj["__diting_body_b64"] = "1";
  const hdrs = JSON.stringify(hdrObj);
  const fetchMode = init.mode || (input instanceof Request ? input.mode : "cors");
  const fetchCredentials = init.credentials !== undefined
    ? String(init.credentials)
    : (input instanceof Request ? input.credentials : "same-origin");
  if (fetchCredentials !== "omit" && fetchCredentials !== "same-origin" && fetchCredentials !== "include") {
    throw new TypeError("Failed to execute 'fetch': '" + fetchCredentials + "' is not a valid RequestCredentials value");
  }
  const pageOrigin = (function() { try { const u = new URL(_domParse("document_url") || "about:blank"); return u.origin; } catch(e) { return ""; } })();
  const raw = await _OPS.op_fetch_url(url, method, hdrs, body, pageOrigin, fetchMode, fetchCredentials);
  const parsed = JSON.parse(raw);
  if (parsed.blocked) {
    const err = new TypeError('net::ERR_FAILED');
    err.name = 'AbortError';
    err.__aborted = true;
    throw err;
  }
  if (parsed.corsBlocked) {
    throw new TypeError('Failed to fetch: ' + (parsed.corsError || 'CORS error'));
  }
  const respType = parsed.status === 0 ? "opaque" : (fetchMode === "no-cors" ? "opaque" : "basic");
  const responseBody = parsed.bodyBase64 ? _base64ToUint8Array(parsed.bodyBase64) : (parsed.body || "");
  return new Response(responseBody, {
    status: parsed.status,
    statusText: "",
    headers: parsed.headers || {},
    type: respType,
    url: parsed.url || url,
    redirected: false,
  });
};

if (typeof Headers === "undefined") {
  globalThis.Headers = class Headers {
    constructor(init={}) { this._h={}; if(init) { if(init instanceof Headers) { init.forEach((v,k)=>{this._h[k]=v;}); } else if(typeof init==="object") { for(const[k,v]of Object.entries(init)) this._h[k.toLowerCase()]=String(v); } } }
    get(n) { return this._h[n.toLowerCase()]??null; } set(n,v) { this._h[n.toLowerCase()]=String(v); }
    has(n) { return n.toLowerCase() in this._h; } delete(n) { delete this._h[n.toLowerCase()]; }
    append(n,v) { this._h[n.toLowerCase()]=String(v); }
    forEach(cb) { for(const[k,v] of Object.entries(this._h)) cb(v,k,this); }
    entries() { return Object.entries(this._h)[Symbol.iterator](); }
    keys() { return Object.keys(this._h)[Symbol.iterator](); }
    values() { return Object.values(this._h)[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
  };
}

// XMLHttpRequestEventTarget — spec-required ancestor for XHR EventTarget methods.
// zone.js prefers to walk XMLHttpRequestEventTarget.prototype for addEventListener/
// removeEventListener/dispatchEvent descriptors before falling back to XHR.prototype.
class XMLHttpRequestEventTarget {
  addEventListener(type, handler) {
    if (!this._listeners) this._listeners = {};
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(handler);
  }
  removeEventListener(type, handler) {
    if (this._listeners && this._listeners[type]) {
      this._listeners[type] = this._listeners[type].filter(h => h !== handler);
    }
  }
  dispatchEvent(event) {
    if (!event || !event.type) return false;
    const ev = (typeof event === 'object') ? event : { type: event };
    ev.target = ev.target || this;
    ev.currentTarget = ev.currentTarget || this;
    const type = ev.type;
    const handlers = (this._listeners && this._listeners[type]) || [];
    for (const h of handlers) { try { h.call(this, ev); } catch (e) {} }
    const prop = 'on' + type;
    if (typeof this[prop] === 'function') {
      try { this[prop](ev); } catch (e) {}
    }
    return true;
  }
}
globalThis.XMLHttpRequestEventTarget = XMLHttpRequestEventTarget;
_markNative(XMLHttpRequestEventTarget);
_markNative(XMLHttpRequestEventTarget.prototype.addEventListener);
_markNative(XMLHttpRequestEventTarget.prototype.removeEventListener);
_markNative(XMLHttpRequestEventTarget.prototype.dispatchEvent);

globalThis.XMLHttpRequest = class XMLHttpRequest extends XMLHttpRequestEventTarget {
  static UNSENT = 0;
  static OPENED = 1;
  static HEADERS_RECEIVED = 2;
  static LOADING = 3;
  static DONE = 4;
  UNSENT = 0; OPENED = 1; HEADERS_RECEIVED = 2; LOADING = 3; DONE = 4;

  constructor() {
    super();
    this.readyState = 0;
    this.status = 0;
    this.statusText = "";
    this.responseText = "";
    this.responseXML = null;
    this.responseURL = "";
    this.responseType = "";
    this.response = null;
    this.timeout = 0;
    this.withCredentials = false;
    this.upload = { addEventListener(){}, removeEventListener(){} };
    this._method = "GET";
    this._url = "";
    this._headers = {};
    this._responseHeaders = {};
    this._aborted = false;
    this._listeners = {};
    this.onreadystatechange = null;
    this.onload = null;
    this.onerror = null;
    this.onabort = null;
    this.onprogress = null;
    this.ontimeout = null;
    this.onloadstart = null;
    this.onloadend = null;
  }

  open(method, url, async_) {
    this._method = method;
    this._url = url;
    this._headers = {};
    this._responseHeaders = {};
    this._aborted = false;
    this.status = 0;
    this.statusText = "";
    this.responseText = "";
    this.response = null;
    this._setReadyState(1);
  }

  setRequestHeader(name, value) {
    this._headers[name] = value;
  }

  getResponseHeader(name) {
    const lower = name.toLowerCase();
    for (const [k, v] of Object.entries(this._responseHeaders)) {
      if (k.toLowerCase() === lower) return v;
    }
    return null;
  }

  getAllResponseHeaders() {
    return Object.entries(this._responseHeaders)
      .map(([k, v]) => k + ': ' + v)
      .join('\r\n');
  }

  overrideMimeType(mime) { this._overrideMime = mime; }

  send(body) {
    if (this.readyState !== 1) return;
    if (this._aborted) return;

    const xhr = this;
    this._fireEvent('loadstart');

    let url = this._url;
    if (url && !url.includes('://')) {
      try {
        const base = _docBase();
        url = new URL(url, base).href;
      } catch(e) {}
    }

    fetch(url, {
      method: this._method,
      headers: this._headers,
      body: body || undefined,
      mode: 'cors',
      credentials: this.withCredentials ? 'include' : 'same-origin',
    }).then(async (resp) => {
      if (xhr._aborted) return;

      xhr.status = resp.status;
      xhr.statusText = resp.statusText || '';
      xhr.responseURL = resp.url || url;

      if (resp.headers) {
        resp.headers.forEach((v, k) => { xhr._responseHeaders[k] = v; });
      }

      xhr._setReadyState(2); // HEADERS_RECEIVED

      const text = await resp.text();
      if (xhr._aborted) return;

      xhr.responseText = text;
      xhr._setReadyState(3); // LOADING

      switch (xhr.responseType) {
        case 'json':
          try { xhr.response = JSON.parse(text); } catch(e) { xhr.response = null; }
          break;
        case 'text':
        case '':
          xhr.response = text;
          break;
        case 'arraybuffer':
          xhr.response = new TextEncoder().encode(text).buffer;
          break;
        case 'blob':
          xhr.response = new Blob([text]);
          break;
        case 'document':
          xhr.response = text; // simplified
          break;
        default:
          xhr.response = text;
      }

      xhr._setReadyState(4); // DONE
      xhr._fireEvent('load');
      xhr._fireEvent('loadend');
    }).catch((err) => {
      if (xhr._aborted) return;
      xhr.status = 0;
      xhr.readyState = 4;
      xhr._fireEvent('readystatechange');
      if (err && err.__aborted) {
        xhr._aborted = true;
        xhr._fireEvent('abort');
        xhr._fireEvent('loadend');
        if (xhr.onabort) xhr.onabort(err);
      } else {
        xhr._fireEvent('error');
        xhr._fireEvent('loadend');
        if (xhr.onerror) xhr.onerror(err);
      }
    });
  }

  abort() {
    this._aborted = true;
    if (this.readyState > 0 && this.readyState < 4) {
      this._setReadyState(4);
      this._fireEvent('abort');
      this._fireEvent('loadend');
    }
    this.readyState = 0;
  }

  addEventListener(type, handler) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(handler);
  }

  removeEventListener(type, handler) {
    if (this._listeners[type]) {
      this._listeners[type] = this._listeners[type].filter(h => h !== handler);
    }
  }

  // Per WHATWG DOM spec — required by zone.js which patches XHR via
  // Object.getOwnPropertyDescriptor on XMLHttpRequestEventTarget.prototype.
  dispatchEvent(event) {
    if (!event || !event.type) return false;
    const ev = (typeof event === 'object') ? event : { type: event };
    ev.target = ev.target || this;
    ev.currentTarget = ev.currentTarget || this;
    const type = ev.type;
    const handlers = (this._listeners && this._listeners[type]) || [];
    for (const h of handlers) { try { h.call(this, ev); } catch (e) {} }
    const prop = 'on' + type;
    if (typeof this[prop] === 'function') {
      try { this[prop](ev); } catch (e) {}
    }
    return true;
  }

  _setReadyState(state) {
    this.readyState = state;
    this._fireEvent('readystatechange');
    if (this.onreadystatechange) {
      try { this.onreadystatechange(); } catch(e) {}
    }
  }

  _fireEvent(type) {
    const event = { type, target: this, currentTarget: this, bubbles: false };
    const handlers = this._listeners[type] || [];
    for (const h of handlers) { try { h.call(this, event); } catch(e) {} }
    const prop = 'on' + type;
    if (type !== 'readystatechange' && typeof this[prop] === 'function') {
      try { this[prop](event); } catch(e) {}
    }
  }
};
_markNative(XMLHttpRequest);
_markNative(XMLHttpRequest.prototype.open);
_markNative(XMLHttpRequest.prototype.send);
_markNative(XMLHttpRequest.prototype.abort);
_markNative(XMLHttpRequest.prototype.setRequestHeader);
_markNative(XMLHttpRequest.prototype.addEventListener);
_markNative(XMLHttpRequest.prototype.removeEventListener);
_markNative(XMLHttpRequest.prototype.dispatchEvent);
_markNative(XMLHttpRequest.prototype.getResponseHeader);
_markNative(XMLHttpRequest.prototype.getAllResponseHeaders);

// WHATWG URL parsing/serialization is delegated to the Rust `url` crate via
// op_url_parse / op_url_set. The op returns the full component set as JSON; the
// constructor caches it so getters are plain field reads (no per-access op) and
// the hot paths (navigation, fetch, _resolveUrl) stay cheap. Returns null when
// the input is not a valid URL.
function _urlParseOp(url, base) {
  try {
    const s = _OPS.op_url_parse(String(url), (base === undefined || base === null) ? "" : String(base));
    const c = JSON.parse(s);
    return (c && c.ok) ? c : null;
  } catch (e) { return null; }
}
function _urlSetOp(href, part, value) {
  try {
    const s = _OPS.op_url_set(String(href), part, String(value));
    const c = JSON.parse(s);
    return (c && c.ok) ? c : null;
  } catch (e) { return null; }
}
// Returns just the resolved absolute URL string (no component JSON), or null on
// failure. Cheaper than _urlParseOp for callers that only need the href.
function _urlResolveOp(href, base) {
  try {
    const r = _OPS.op_url_resolve(String(href), (base === undefined || base === null) ? "" : String(base));
    return r ? r : null;
  } catch (e) { return null; }
}
if (typeof URL === 'undefined' || !URL.prototype || !URL.__diting) {
  const _URL = class URL {
    constructor(url, base) {
      const c = _urlParseOp(url, base);
      if (!c) throw new TypeError("Failed to construct 'URL': Invalid URL");
      this._c = c;
      this._sp = null;
    }
    get href() { return this._c.href; }
    set href(v) { const c = _urlParseOp(v, undefined); if (!c) throw new TypeError("Failed to set the 'href' property on 'URL': Invalid URL"); this._c = c; this._refreshSP(); }
    get protocol() { return this._c.protocol; }
    set protocol(v) { this._set('protocol', v); }
    get username() { return this._c.username; }
    set username(v) { this._set('username', v); }
    get password() { return this._c.password; }
    set password(v) { this._set('password', v); }
    get host() { return this._c.host; }
    set host(v) { this._set('host', v); }
    get hostname() { return this._c.hostname; }
    set hostname(v) { this._set('hostname', v); }
    get port() { return this._c.port; }
    set port(v) { this._set('port', v); }
    get pathname() { return this._c.pathname; }
    set pathname(v) { this._set('pathname', v); }
    get search() { return this._c.search; }
    set search(v) { this._set('search', v); this._refreshSP(); }
    get hash() { return this._c.hash; }
    set hash(v) { this._set('hash', v); }
    get origin() { return this._c.origin; }
    get searchParams() {
      if (!this._sp) { this._sp = new URLSearchParams(this._c.search); this._sp._url = this; }
      return this._sp;
    }
    _set(part, value) { const c = _urlSetOp(this._c.href, part, value); if (c) this._c = c; }
    // search changed on the URL side: refresh the bound searchParams contents.
    _refreshSP() { if (this._sp && this._sp._setFromString) this._sp._setFromString(this._c.search); }
    // searchParams mutated: write the serialized query back without re-refreshing.
    _updateSearch(qs) { this._set('search', qs ? ('?' + qs) : ''); }
    toString() { return this._c.href; }
    toJSON() { return this._c.href; }
    static createObjectURL() { return 'blob:null/fake-' + Math.random().toString(36).slice(2); }
    static revokeObjectURL() {}
    // WHATWG URL.parse: like the constructor but returns null instead of throwing.
    static parse(url, base) { const c = _urlParseOp(url, base); if (!c) return null; const u = Object.create(_URL.prototype); u._c = c; u._sp = null; return u; }
    static canParse(url, base) { return _urlParseOp(url, base) !== null; }
  };
  _URL.__diting = true;
  globalThis.URL = _URL;
}

globalThis.requestIdleCallback = globalThis.requestIdleCallback || function requestIdleCallback(cb, opts) {
  const start = Date.now();
  return setTimeout(() => {
    cb({
      didTimeout: false,
      timeRemaining() { return Math.max(0, 50 - (Date.now() - start)); },
    });
  }, 1);
};
globalThis.cancelIdleCallback = globalThis.cancelIdleCallback || function cancelIdleCallback(id) { clearTimeout(id); };
_markNative(globalThis.requestIdleCallback);
_markNative(globalThis.cancelIdleCallback);

if (typeof Request === 'undefined') {
  globalThis.Request = class Request {
    constructor(input, init = {}) {
      const inputRequest = input instanceof Request ? input : null;
      if (typeof input === 'string') { this.url = input; }
      else if (inputRequest) { this.url = inputRequest.url; init = { ...inputRequest, ...init }; }
      else if (typeof URL === 'function' && input instanceof URL) { this.url = input.href; }
      else { this.url = input?.url || input?.href || String(input); }
      this.method = (init.method || 'GET').toUpperCase();
      this.headers = new Headers(init.headers);
      this.body = init.body || null;
      this.mode = init.mode || 'cors';
      this.credentials = init.credentials !== undefined
        ? String(init.credentials)
        : (inputRequest ? inputRequest.credentials : 'same-origin');
      if (this.credentials !== 'omit' && this.credentials !== 'same-origin' && this.credentials !== 'include') {
        throw new TypeError("Failed to construct 'Request': '" + this.credentials + "' is not a valid RequestCredentials value");
      }
      this.redirect = init.redirect || 'follow';
      this.referrer = init.referrer || '';
      this.signal = init.signal || { aborted: false, addEventListener(){}, removeEventListener(){} };
      this.cache = init.cache || 'default';
    }
    clone() {
      return new Request(this.url, {
        method: this.method,
        headers: this.headers,
        body: this.body,
        mode: this.mode,
        credentials: this.credentials,
        redirect: this.redirect,
        referrer: this.referrer,
        signal: this.signal,
        cache: this.cache,
      });
    }
    async text() { return this.body ? String(this.body) : ''; }
    async json() { return JSON.parse(await this.text()); }
    async arrayBuffer() { return new TextEncoder().encode(await this.text()).buffer; }
    async blob() {
      const ct = this.headers && this.headers.get ? (this.headers.get('content-type') || '') : '';
      return new Blob(this.body != null ? [this.body] : [], { type: ct });
    }
  };
}

// Decode a response body honoring the Content-Type charset, so fetch()/XHR
// over non-UTF-8 resources (GBK, Shift_JIS, ISO-8859-x, ...) return correctly
// decoded text instead of mojibake. The UTF-8 case (the overwhelming majority)
// takes the plain TextDecoder fast path; only an explicit non-UTF-8 charset
// routes through TextDecoder(label), which falls back to UTF-8 on a bad label.
function _decodeBodyWithCharset(bytes, headers) {
  let label = '';
  try {
    const ct = headers && typeof headers.get === 'function' ? (headers.get('content-type') || '') : '';
    const m = /charset\s*=\s*"?([^";]+)"?/i.exec(ct);
    if (m) label = m[1].trim();
  } catch (e) {}
  if (!label || /^utf-?8$/i.test(label)) return new TextDecoder().decode(bytes);
  try { return new TextDecoder(label).decode(bytes); }
  catch (e) { return new TextDecoder().decode(bytes); }
}

if (typeof Response === 'undefined') {
  globalThis.Response = class Response {
    constructor(body, init = {}) {
      this._bodyBytes = _bodyToUint8Array(body); this.status = init.status || 200; this.statusText = init.statusText || '';
      this.ok = this.status >= 200 && this.status < 300;
      this.headers = new Headers(init.headers);
      this.type = init.type || 'basic'; this.url = init.url || ''; this.redirected = !!init.redirected;
    }
    async text() { this._bodyUsed = true; return _decodeBodyWithCharset(this._bodyBytes, this.headers); }
    async json() { this._bodyUsed = true; return JSON.parse(await this.text()); }
    async arrayBuffer() { this._bodyUsed = true; return _arrayBufferFromBytes(this._bodyBytes); }
    async blob() { this._bodyUsed = true; return new Blob([this._bodyBytes]); }
    // RSC/flight consumers (React server actions, Next app-router prefetch)
    // stream the payload through response.body.getReader(); without a body
    // stream createFromFetch resolves `x.body` to undefined and the whole
    // action/navigation promise chain hangs forever — the "server accepted
    // the POST but the client never navigates" wedge. Always return a
    // stream (empty+closed when there are no bytes) rather than null: the
    // lazy-stream wrapper in the flight client cannot cope with null.
    get body() {
      if (!this._bodyStream) {
        const bytes = this._bodyBytes;
        this._bodyStream = new ReadableStream({
          start(c) { if (bytes && bytes.length) c.enqueue(new Uint8Array(bytes)); c.close(); },
        });
      }
      return this._bodyStream;
    }
    get bodyUsed() { return this._bodyUsed === true; }
    clone() { return new Response(this._bodyBytes, { status: this.status, statusText: this.statusText, headers: this.headers, type: this.type, url: this.url, redirected: this.redirected }); }
    static error() { return new Response(null, { status: 0 }); }
    static redirect(url, status) { return new Response(null, { status: status || 302, headers: { Location: url } }); }
    static json(data, init) { return new Response(JSON.stringify(data), { ...init, headers: { 'content-type': 'application/json', ...(init?.headers || {}) } }); }
  };
}

if (!Element.prototype.replaceWith) {
  Element.prototype.replaceWith = function(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    for (const n of nodes) {
      if (typeof n === 'string') parent.insertBefore(document.createTextNode(n), this);
      else parent.insertBefore(n, this);
    }
    parent.removeChild(this);
  };
  _markNative(Element.prototype.replaceWith);
}
if (!Element.prototype.before) {
  Element.prototype.before = function(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    for (const n of nodes) {
      if (typeof n === 'string') parent.insertBefore(document.createTextNode(n), this);
      else parent.insertBefore(n, this);
    }
  };
  _markNative(Element.prototype.before);
}
if (!Element.prototype.after) {
  Element.prototype.after = function(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    const ref = this.nextSibling;
    for (const n of nodes) {
      if (typeof n === 'string') parent.insertBefore(document.createTextNode(n), ref);
      else parent.insertBefore(n, ref);
    }
  };
  _markNative(Element.prototype.after);
}

// ChildNode mixin: also mix before/after/replaceWith/remove into
// CharacterData.prototype (covers Text, Comment, ProcessingInstruction).
// These are the same implementations as Element.prototype — frameworks
// (Svelte 5, Vue, Lit) anchor on Comment/Text nodes and call these methods.
if (!CharacterData.prototype.before) CharacterData.prototype.before = Element.prototype.before;
if (!CharacterData.prototype.after) CharacterData.prototype.after = Element.prototype.after;
if (!CharacterData.prototype.replaceWith) CharacterData.prototype.replaceWith = Element.prototype.replaceWith;
if (!CharacterData.prototype.remove) CharacterData.prototype.remove = Element.prototype.remove;

if (!('isConnected' in Node.prototype)) {
  Object.defineProperty(Node.prototype, 'isConnected', {
    get() {
      let node = this;
      while (node) {
        if (node.nodeType === 9) return true; // Document node
        node = node.parentNode;
      }
      return false;
    }
  });
}

globalThis.ResizeObserver = class ResizeObserver {
  constructor(callback) {
    this._callback = callback;
    this._targets = new Set();
    this._connected = true;
    this._fireCount = 0;
  }
  _fireFor(targets) {
    if (!this._connected || !targets.length) return;
    const records = targets.map(target => {
      const r = target.getBoundingClientRect ? target.getBoundingClientRect() : { x: 0, y: 0, width: 100, height: 20 };
      return {
        target,
        contentRect: { x: r.x || 0, y: r.y || 0, width: r.width || 100, height: r.height || 20, top: r.top || 0, left: r.left || 0, bottom: r.bottom || 20, right: r.right || 100 },
        borderBoxSize: [{ blockSize: r.height || 20, inlineSize: r.width || 100 }],
        contentBoxSize: [{ blockSize: r.height || 20, inlineSize: r.width || 100 }],
        devicePixelContentBoxSize: [{ blockSize: r.height || 20, inlineSize: r.width || 100 }],
      };
    });
    try { this._callback(records, this); } catch (e) { /* RO callbacks must not propagate */ }
  }
  observe(el) {
    if (!el || !this._connected) return;
    if (this._targets.has(el)) return;
    this._targets.add(el);
    Promise.resolve().then(() => this._fireFor([el]));
    [200, 800].forEach(delay => {
      setTimeout(() => {
        if (this._connected && this._targets.has(el) && this._fireCount < 16) {
          this._fireCount++;
          this._fireFor([el]);
        }
      }, delay);
    });
  }
  unobserve(el) { this._targets.delete(el); }
  disconnect() { this._connected = false; this._targets.clear(); }
};

if (typeof TextEncoder === 'undefined') {
  globalThis.TextEncoder = class TextEncoder {
    get encoding() { return 'utf-8'; }
    encode(str) {
      str = String(str);
      const buf = [];
      for (let i = 0; i < str.length; i++) {
        let c = str.charCodeAt(i);
        if (c < 0x80) buf.push(c);
        else if (c < 0x800) { buf.push(0xC0|(c>>6), 0x80|(c&0x3F)); }
        else if (c < 0xD800 || c >= 0xE000) { buf.push(0xE0|(c>>12), 0x80|((c>>6)&0x3F), 0x80|(c&0x3F)); }
        else { c = 0x10000 + (((c & 0x3FF) << 10) | (str.charCodeAt(++i) & 0x3FF)); buf.push(0xF0|(c>>18), 0x80|((c>>12)&0x3F), 0x80|((c>>6)&0x3F), 0x80|(c&0x3F)); }
      }
      return new Uint8Array(buf);
    }
    encodeInto(str, dest) { const enc = this.encode(str); dest.set(enc.slice(0, dest.length)); return { read: str.length, written: Math.min(enc.length, dest.length) }; }
  };
}
// Fast pure-JS UTF-8 decode (the common case: Response/Blob .text(), most
// pages). Avoids the op + JSON round trip for plain UTF-8.
function _utf8DecodeBytes(bytes, start) {
  let str = '', i = start | 0;
  const n = bytes.length;
  while (i < n) {
    let c = bytes[i++];
    if (c < 0x80) str += String.fromCharCode(c);
    else if (c < 0xE0) str += String.fromCharCode(((c & 0x1F) << 6) | (bytes[i++] & 0x3F));
    else if (c < 0xF0) { const b1 = bytes[i++], b2 = bytes[i++]; str += String.fromCharCode(((c & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F)); }
    else { const b1 = bytes[i++], b2 = bytes[i++], b3 = bytes[i++]; const cp = ((c & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F); if (cp > 0xFFFF) { const s = cp - 0x10000; str += String.fromCharCode(0xD800 + (s >> 10), 0xDC00 + (s & 0x3FF)); } else str += String.fromCharCode(cp); }
  }
  return str;
}
if (typeof TextDecoder === 'undefined') {
  globalThis.TextDecoder = class TextDecoder {
    constructor(label, options) {
      // No-arg construction (Response.text()/Blob.text() and most pages) is
      // UTF-8; skip the label-validation op on that hot path.
      let name;
      if (label === undefined) {
        name = 'utf-8';
      } else {
        name = _OPS.op_encoding_for_label(String(label));
        if (!name) throw new RangeError("Failed to construct 'TextDecoder': The encoding label provided ('" + label + "') is invalid.");
      }
      const o = options || {};
      Object.defineProperty(this, 'encoding', { value: name, enumerable: true });
      Object.defineProperty(this, 'fatal', { value: !!o.fatal, enumerable: true });
      Object.defineProperty(this, 'ignoreBOM', { value: !!o.ignoreBOM, enumerable: true });
      // Bytes carried over between decode(..., {stream:true}) calls
      // (TextDecoderStream splits chunks at arbitrary byte offsets).
      this._pending = new Uint8Array(0);
      this._bomChecked = false;
    }
    decode(input, options) {
      const stream = !!(options && options.stream);
      let bytes = input === undefined
        ? new Uint8Array(0)
        : ArrayBuffer.isView(input)
          ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
          : new Uint8Array(input);
      if (this._pending.length || stream) {
        const merged = new Uint8Array(this._pending.length + bytes.length);
        merged.set(this._pending);
        merged.set(bytes, this._pending.length);
        bytes = merged;
        if (stream) {
          if (this.encoding === 'utf-8' && !this.fatal) {
            // Emit only complete UTF-8 sequences; hold the incomplete tail.
            let cut = bytes.length;
            for (let i = bytes.length - 1; i >= 0 && i >= bytes.length - 4; i--) {
              const b = bytes[i];
              if ((b & 0xC0) === 0x80) continue;
              const need = b < 0x80 ? 0 : b < 0xE0 ? 1 : b < 0xF0 ? 2 : 3;
              if (i + need + 1 > bytes.length) cut = i;
              break;
            }
            this._pending = bytes.slice(cut);
            bytes = bytes.slice(0, cut);
          } else {
            // Legacy encodings / fatal mode: withhold output until flush.
            this._pending = bytes;
            return '';
          }
        } else {
          this._pending = new Uint8Array(0);
        }
      }
      if (!stream) this._pending = new Uint8Array(0);
      // Fast path: plain UTF-8, non-fatal (Response/Blob text, most pages).
      if (this.encoding === 'utf-8' && !this.fatal) {
        let off = 0;
        if (!this._bomChecked && bytes.length) {
          this._bomChecked = true;
          if (!this.ignoreBOM && bytes.length >= 3 && bytes[0] === 0xEF && bytes[1] === 0xBB && bytes[2] === 0xBF) off = 3;
        }
        return _utf8DecodeBytes(bytes, off);
      }
      // Legacy encodings / fatal mode: encoding_rs via the op.
      const r = JSON.parse(_OPS.op_text_decode(this.encoding, bytes, this.fatal, this.ignoreBOM));
      if (!r.ok) throw new TypeError("Failed to execute 'decode' on 'TextDecoder': The encoded data was not valid.");
      return r.v;
    }
  };
}

// React Router's SSR stream bootstrap does
// `new ReadableStream(...).pipeThrough(new TextEncoderStream())`; without
// these, hydration never runs and the whole client app is inert.
// Composition, not `extends TransformStream`: TransformStream is installed
// by a deno extension AFTER the snapshot is taken, so it does not exist yet
// when this file is evaluated at build time.
if (typeof TextEncoderStream === 'undefined') {
  globalThis.TextEncoderStream = class TextEncoderStream {
    constructor() {
      let pending = '';
      const enc = new TextEncoder();
      const ts = new TransformStream({
        transform(chunk, controller) {
          chunk = pending + String(chunk);
          pending = '';
          const last = chunk.charCodeAt(chunk.length - 1);
          if (last >= 0xD800 && last < 0xDC00) {
            pending = chunk[chunk.length - 1];
            chunk = chunk.slice(0, -1);
          }
          if (chunk) controller.enqueue(enc.encode(chunk));
        },
        flush(controller) {
          if (pending) controller.enqueue(enc.encode(pending));
        }
      });
      this.readable = ts.readable;
      this.writable = ts.writable;
    }
    get encoding() { return 'utf-8'; }
  };
  globalThis.TextDecoderStream = class TextDecoderStream {
    constructor(label, options) {
      const dec = new TextDecoder(label, options);
      const ts = new TransformStream({
        transform(chunk, controller) {
          const out = dec.decode(chunk, { stream: true });
          if (out) controller.enqueue(out);
        },
        flush(controller) {
          const out = dec.decode();
          if (out) controller.enqueue(out);
        }
      });
      this.readable = ts.readable;
      this.writable = ts.writable;
      Object.defineProperty(this, 'encoding', { value: dec.encoding, enumerable: true });
      Object.defineProperty(this, 'fatal', { value: dec.fatal, enumerable: true });
      Object.defineProperty(this, 'ignoreBOM', { value: dec.ignoreBOM, enumerable: true });
    }
  };
}

// matchMedia evaluates against the live window viewport — the same
// innerWidth/innerHeight the persona publishes and set_viewport feeds the
// layout ICB — so a page's JS branching agrees with the @media rules the CSS
// cascade already applied. The old always-false stub answered
// `(min-width:640px)` with false while a 2560px viewport was published:
// wrong for responsive pages and a self-contradiction any fingerprint
// script could catch by cross-checking the two. Covers what page scripts
// actually branch on: width/height bounds, orientation, aspect-ratio,
// resolution, and the preference features pinned to the desktop-light
// persona. Comma (or) / and / not follow the classic grammar; unknown
// features evaluate false, matching Chrome's answer for unsupported ones.
const _MQ_PERSONA_BOOL = {
  'prefers-color-scheme': { light: true, dark: false },
  'prefers-reduced-motion': { 'no-preference': true, reduce: false },
  'prefers-reduced-transparency': { 'no-preference': true, reduce: false },
  pointer: { fine: true, coarse: false, none: false },
  'any-pointer': { fine: true, coarse: false, none: false },
  hover: { hover: true, none: false },
  'any-hover': { hover: true, none: false },
};
function _mqExpr(inner) {
  const colon = inner.indexOf(':');
  const feature = (colon < 0 ? inner : inner.slice(0, colon)).trim().toLowerCase();
  const value = colon < 0 ? null : inner.slice(colon + 1).trim().toLowerCase();
  if (feature in _MQ_PERSONA_BOOL) return value != null && _MQ_PERSONA_BOOL[feature][value] === true;
  if (value == null) return false;
  const num = parseFloat(value);
  const vw = globalThis.innerWidth || 0, vh = globalThis.innerHeight || 0;
  switch (feature) {
    case 'min-width': return num <= vw;
    case 'max-width': return num >= vw;
    case 'width': return num === vw;
    case 'min-height': return num <= vh;
    case 'max-height': return num >= vh;
    case 'height': return num === vh;
    case 'orientation': return value === (vw >= vh ? 'landscape' : 'portrait');
    case 'aspect-ratio': case 'min-aspect-ratio': case 'max-aspect-ratio': {
      const m = value.match(/^(\d*\.?\d+)\s*\/\s*(\d*\.?\d+)$/);
      if (!m) return false;
      const want = parseFloat(m[1]) / parseFloat(m[2]), have = vw / vh;
      return feature === 'aspect-ratio' ? Math.abs(have - want) < 1e-6
        : feature === 'min-aspect-ratio' ? have >= want : have <= want;
    }
    case 'resolution': case 'min-resolution': case 'max-resolution': {
      // Normalize the value to device px per css px (dppx) and compare
      // against the persona's devicePixelRatio.
      let dppx;
      if (/dppx|x$/.test(value)) dppx = num;
      else if (/dpcm$/.test(value)) dppx = (num * 96) / 2.54;
      else dppx = num / 96; // dpi; a unitless number is invalid CSS, dpi is the least-wrong read
      const dpr = globalThis.devicePixelRatio || 1;
      return feature === 'resolution' ? Math.abs(dppx - dpr) < 1e-6
        : feature === 'min-resolution' ? dppx <= dpr : dppx >= dpr;
    }
    default: return false;
  }
}
function _mqClause(clause) {
  let s = clause.trim(), invert = false;
  if (/^not\s+/i.test(s)) { invert = true; s = s.replace(/^not\s+/i, ''); }
  s = s.replace(/^only\s+/i, '');
  const parts = s.split(/\s+and\s+/i);
  let i = 0;
  if (parts[0] && parts[0][0] !== '(') {
    const mt = parts[0].toLowerCase();
    if (mt !== 'all' && mt !== 'screen') return invert;
    i = 1;
  }
  for (; i < parts.length; i++) {
    const p = parts[i].trim();
    if (!p) continue;
    if (!(p[0] === '(' && p[p.length - 1] === ')') || !_mqExpr(p.slice(1, -1))) return invert;
  }
  return !invert;
}
function _mqMatches(q) {
  const s = String(q).trim();
  if (s === '') return true;
  const clauses = [];
  let depth = 0, cur = '';
  for (const ch of s) {
    if (ch === '(') depth++;
    else if (ch === ')') depth--;
    if (ch === ',' && depth === 0) { clauses.push(cur); cur = ''; continue; }
    cur += ch;
  }
  clauses.push(cur);
  return clauses.some((c) => c.trim() !== '' && _mqClause(c));
}
globalThis.matchMedia = _markNative(function matchMedia(q) {
  let matches = false;
  try { matches = _mqMatches(q); } catch (e) {}
  return { matches, media: String(q), onchange: null,
           addListener(){}, removeListener(){}, addEventListener(){}, removeEventListener(){}, dispatchEvent(){return true;} };
});
// Chrome's enumerable computed-style property set (kebab-case). Real
// Chrome 145 exposes ~470 of these on every getComputedStyle() result;
// count AND enumeration are fingerprint surfaces (Castle cssKeys).
const _COMPUTED_PROPS_KEBAB = ('accent-color align-content align-items align-self alignment-baseline all animation animation-composition ' +
  'animation-delay animation-direction animation-duration animation-fill-mode animation-iteration-count animation-name animation-play-state ' +
  'animation-range animation-range-end animation-range-start animation-timeline app-region appearance ascent-override aspect-ratio ' +
  'backdrop-filter backface-visibility background background-attachment background-blend-mode background-clip background-color ' +
  'background-image background-origin background-position background-position-x background-position-y background-repeat background-size ' +
  'baseline-shift block-size border border-block border-block-color border-block-end border-block-end-color border-block-end-style ' +
  'border-block-end-width border-block-start border-block-start-color border-block-start-style border-block-start-width border-block-style ' +
  'border-block-width border-bottom border-bottom-color border-bottom-left-radius border-bottom-right-radius border-bottom-style ' +
  'border-bottom-width border-collapse border-color border-end-end-radius border-end-start-radius border-image border-image-outset ' +
  'border-image-repeat border-image-slice border-image-source border-image-width border-left border-left-color border-left-style ' +
  'border-left-width border-radius border-right border-right-color border-right-style border-right-width border-spacing ' +
  'border-start-end-radius border-start-start-radius border-style border-top border-top-color border-top-left-radius ' +
  'border-top-right-radius border-top-style border-top-width border-width bottom box-decoration-break box-shadow box-sizing break-after ' +
  'break-before break-inside buffered-rendering caption-side caret caret-color caret-shape clear clip clip-path clip-rule color ' +
  'color-interpolation color-interpolation-filters color-scheme column-count column-fill column-gap column-rule column-rule-color ' +
  'column-rule-style column-rule-width column-span column-width columns contain contain-intrinsic-block-size contain-intrinsic-height ' +
  'contain-intrinsic-inline-size contain-intrinsic-size contain-intrinsic-width container container-name container-type content ' +
  'content-visibility counter-increment counter-reset counter-set cursor cx cy d descent-override direction display dominant-baseline ' +
  'empty-cells field-sizing fill fill-opacity fill-rule filter flex flex-basis flex-direction flex-flow flex-grow flex-shrink flex-wrap ' +
  'float flood-color flood-opacity font font-family font-feature-settings font-kerning font-optical-sizing font-palette font-size ' +
  'font-size-adjust font-stretch font-style font-synthesis font-synthesis-position font-synthesis-small-caps font-synthesis-style ' +
  'font-synthesis-weight font-variant font-variant-alternates font-variant-caps font-variant-east-asian font-variant-emoji ' +
  'font-variant-ligatures font-variant-numeric font-variant-position font-variation-settings font-weight forced-color-adjust gap ' +
  'glyph-orientation-horizontal glyph-orientation-vertical grid grid-area grid-auto-columns grid-auto-flow grid-auto-rows grid-column ' +
  'grid-column-end grid-column-gap grid-column-start grid-row grid-row-end grid-row-gap grid-row-start grid-template grid-template-areas ' +
  'grid-template-columns grid-template-rows hanging-punctuation height hyphenate-character hyphenate-limit-chars hyphens ' +
  'image-orientation image-rendering image-resolution inherits initial-letter initial-letter-align inline-size input-security inset ' +
  'inset-block inset-block-end inset-block-start inset-inline inset-inline-end inset-inline-start isolation justify-content ' +
  'justify-items justify-self left letter-spacing lighting-color line-break line-height line-height-step list-style list-style-image ' +
  'list-style-position list-style-type margin margin-block margin-block-end margin-block-start margin-bottom margin-inline ' +
  'margin-inline-end margin-inline-start margin-left margin-right margin-top marker marker-end marker-mid marker-start mask mask-border ' +
  'mask-border-mode mask-border-outset mask-border-repeat mask-border-slice mask-border-source mask-border-width mask-clip ' +
  'mask-composite mask-image mask-mode mask-origin mask-position mask-repeat mask-size mask-type math-depth math-shift math-style ' +
  'max-block-size max-height max-inline-size max-width min-block-size min-height min-inline-size min-width mix-blend-mode object-fit ' +
  'object-position offset offset-anchor offset-distance offset-path offset-position offset-rotate opacity order orphans outline ' +
  'outline-color outline-offset outline-style outline-width overflow overflow-anchor overflow-block overflow-clip-margin overflow-inline ' +
  'overflow-wrap overflow-x overflow-y overlay overscroll-behavior overscroll-behavior-block overscroll-behavior-inline ' +
  'overscroll-behavior-x overscroll-behavior-y padding padding-block padding-block-end padding-block-start padding-bottom ' +
  'padding-inline padding-inline-end padding-inline-start padding-left padding-right padding-top page page-break-after page-break-before ' +
  'page-break-inside paint-order perspective perspective-origin place-content place-items place-self pointer-events position ' +
  'print-color-adjust quotes r resize right rotate row-gap ruby-align ruby-position rx ry scale scroll-behavior scroll-margin ' +
  'scroll-margin-block scroll-margin-block-end scroll-margin-block-start scroll-margin-bottom scroll-margin-inline scroll-margin-inline-end ' +
  'scroll-margin-inline-start scroll-margin-left scroll-margin-right scroll-margin-top scroll-padding scroll-padding-block ' +
  'scroll-padding-block-end scroll-padding-block-start scroll-padding-bottom scroll-padding-inline scroll-padding-inline-end ' +
  'scroll-padding-inline-start scroll-padding-left scroll-padding-right scroll-padding-top scroll-snap-align scroll-snap-stop ' +
  'scroll-snap-type scroll-timeline-axis scroll-timeline-name scrollbar-color scrollbar-gutter scrollbar-width shape-image-threshold ' +
  'shape-margin shape-outside shape-rendering speak speak-as tab-size table-layout text-align text-align-last text-anchor ' +
  'text-combine-upright text-decoration text-decoration-color text-decoration-line text-decoration-skip-ink text-decoration-style ' +
  'text-decoration-thickness text-emphasis text-emphasis-color text-emphasis-position text-emphasis-style text-indent text-justify ' +
  'text-orientation text-overflow text-rendering text-shadow text-size-adjust text-transform text-underline-offset ' +
  'text-underline-position text-wrap text-wrap-mode text-wrap-style timeline-scope top touch-action transform transform-box ' +
  'transform-origin transform-style transition transition-behavior transition-delay transition-duration transition-property ' +
  'transition-timing-function translate unicode-bidi user-select vector-effect vertical-align visibility white-space ' +
  'white-space-collapse widows width will-change word-break word-spacing word-wrap writing-mode x y z-index zoom').split(' ');
const _camelCache = new Map();
const _camel = (kebab) => {
  let c = _camelCache.get(kebab);
  if (c === undefined) {
    c = kebab.replace(/-([a-z])/g, (_, ch) => ch.toUpperCase());
    _camelCache.set(kebab, c);
  }
  return c;
};
const _computedKebab = () => _COMPUTED_PROPS_KEBAB;
const _computedSet = () => { if (!_computedSet._s) { _computedSet._s = new Set(_COMPUTED_PROPS_KEBAB.map(_camel)); } return _computedSet._s; };

// getComputedStyle() wrappers share one immutable native snapshot per
// element until the DOM mutates — frameworks call getComputedStyle()
// repeatedly on the same few roots, and jQuery reads one property per
// call, so a fresh snapshot per wrapper would dominate real-page startup
// (upstream learned this the same way).
const _computedStyleSnapshotCache = new WeakMap();
globalThis.getComputedStyle = (el) => {
  if (!el) el = document.body || {};
  const style = el?.style || el?._style || new CSSStyleDeclaration();
  const cacheable = (typeof el === 'object' && el !== null) || typeof el === 'function';
  let snapshot = cacheable ? _computedStyleSnapshotCache.get(el) : null;
  if (!snapshot) {
    snapshot = { rendered: null, epoch: -1 };
    if (cacheable) _computedStyleSnapshotCache.set(el, snapshot);
  }
  // The cascade snapshot from the layout run (whole table per call,
  // upstream's op shape): stylesheet rules + the folded inline style
  // attribute. Absent without a layout run (default build) or for
  // properties outside the engine's table — the fallbacks below keep
  // serving those.
  const refreshRendered = () => {
    if (snapshot.epoch === _ditingMutationEpoch) return;
    snapshot.epoch = _ditingMutationEpoch;
    snapshot.rendered = null;
    if (el?._nid != null) {
      try {
        const raw = _domRaw("computed_style", String(el._nid | 0), "");
        if (raw && raw !== 'null') snapshot.rendered = JSON.parse(raw);
      } catch (e) {}
    }
  };
  // React virtualization libraries (react-window, tanstack-virtual,
  // react-virtuoso) all compute container dimensions via getComputedStyle.
  // The defaults table previously returned `auto` for width/height and
  // `'static'` for position, which made every list render 0 items. Pulling
  // width/height from the synthesized bounding rect makes those libraries
  // actually render content.
  const dimensionFor = (name) => {
    try {
      const r = el.getBoundingClientRect && el.getBoundingClientRect();
      if (!r) return null;
      switch (name) {
        case 'width': case 'inline-size':
          return r.width != null ? `${r.width}px` : null;
        case 'height': case 'block-size':
          return r.height != null ? `${r.height}px` : null;
        case 'left': return r.left != null ? `${r.left}px` : null;
        case 'top': return r.top != null ? `${r.top}px` : null;
        case 'right': return r.right != null ? `${r.right}px` : null;
        case 'bottom': return r.bottom != null ? `${r.bottom}px` : null;
        case 'client-width': case 'offset-width':
          return r.width != null ? `${r.width}px` : null;
        case 'client-height': case 'offset-height':
          return r.height != null ? `${r.height}px` : null;
      }
    } catch (e) {}
    return null;
  };

  const defaultsKebab = {
    display: 'block', visibility: 'visible', opacity: '1',
    position: 'static', overflow: 'visible',
    transform: 'none', 'transform-origin': '0px 0px',
    transition: 'none', animation: 'none',
    float: 'none', clear: 'none',
    margin: '0px', padding: '0px',
    'margin-top': '0px', 'margin-right': '0px', 'margin-bottom': '0px', 'margin-left': '0px',
    'padding-top': '0px', 'padding-right': '0px', 'padding-bottom': '0px', 'padding-left': '0px',
    'font-size': '16px', 'line-height': 'normal', 'font-weight': '400',
    'font-family': 'Times',
    color: 'rgb(0, 0, 0)', 'background-color': 'rgba(0, 0, 0, 0)',
    'border-width': '0px', 'border-style': 'none', 'border-color': 'rgb(0, 0, 0)',
    'border-top-width': '0px', 'border-right-width': '0px',
    'border-bottom-width': '0px', 'border-left-width': '0px',
    'border-radius': '0px',
    'z-index': 'auto', 'pointer-events': 'auto',
    'box-sizing': 'content-box', cursor: 'auto',
    'white-space': 'normal', 'text-align': 'start',
    'flex-direction': 'row', 'flex-wrap': 'nowrap', 'align-items': 'normal',
    'justify-content': 'normal', gap: 'normal',
    'grid-template-columns': 'none', 'grid-template-rows': 'none',
    'will-change': 'auto', 'backface-visibility': 'visible',
  };

  const lookup = (rawProp) => {
    if (typeof rawProp !== 'string') return '';
    // Snapshot first: it already resolved the cascade, inline included.
    refreshRendered();
    const kebab = rawProp.replace(/([A-Z])/g, '-$1').toLowerCase();
    if (snapshot.rendered && Object.prototype.hasOwnProperty.call(snapshot.rendered, kebab)) {
      return snapshot.rendered[kebab];
    }
    // Inline value next — CSSOM writes not yet folded into a snapshot
    // (or no layout run at all).
    const inlineVal = target.getPropertyValue ? target.getPropertyValue(rawProp) : '';
    if (inlineVal) return inlineVal;
    const dim = dimensionFor(kebab);
    if (dim != null) return dim;
    if (defaultsKebab[rawProp]) return defaultsKebab[rawProp];
    if (defaultsKebab[kebab]) return defaultsKebab[kebab];
    return '';
  };

  const target = style;
  return new Proxy(style, {
    get(_, prop) {
      if (prop === Symbol.toPrimitive || prop === Symbol.toStringTag) return undefined;
      // Interface members first — `prop in target` would otherwise answer
      // with the inline style object's own (length: 0, cssText: '') values.
      if (prop === 'getPropertyValue') return (name) => lookup(name);
      if (prop === 'getPropertyPriority') return () => '';
      if (prop === 'item') return (i) => _computedKebab()[i] || '';
      if (prop === 'length') return _computedKebab().length;
      // Indexed access (cs[0], Array.from(cs)) — Chrome answers with the
      // kebab-case property name. Castle's cssKeys collector iterates exactly
      // this way; falling through to lookup() returned '' for every index.
      if (typeof prop === 'string' && prop !== '' && !isNaN(prop) && prop.indexOf('.') === -1) {
        const n = Number(prop);
        if (Number.isInteger(n) && n >= 0 && n < _computedKebab().length) return _computedKebab()[n];
      }
      if (prop === 'cssText') return '';
      if (prop === 'parentRule') return null;
      // CSS property names must resolve through lookup BEFORE the
      // `prop in target` branch: element.style's named-property surface
      // claims every CSS property, so that branch would short-circuit
      // computed reads to the (usually empty) INLINE value —
      // getComputedStyle(el).width answered '' even while
      // getBoundingClientRect().width reported real geometry.
      if (typeof prop === 'string') { const v = lookup(prop); if (v) return v; }
      if (prop in target) return target[prop];
      if (typeof prop === 'string') return lookup(prop);
      return undefined;
    },
    // Real Chrome enumerates ~470 camelCase property names on a computed
    // style (Object.keys / for-in). A featureless object reads as 0 keys,
    // which fingerprinters (Castle cssKeys) score as headless.
    ownKeys() { return _computedKebab().map(_camel); },
    getOwnPropertyDescriptor(_, prop) {
      if (typeof prop === 'string' && _computedSet().has(prop)) {
        return { configurable: true, enumerable: true, get: () => lookup(prop) };
      }
      return Object.getOwnPropertyDescriptor(target, prop);
    },
    has(_, prop) {
      if (typeof prop === 'string' && _computedSet().has(prop)) return true;
      return prop in target;
    },
  });
};
// Returns the one Selection instance for a document (cached on the document),
// so window.getSelection() === document.getSelection(). The real Selection
// class is defined below, after Range. _selectionFor is hoisted.
function _selectionFor(doc) {
  if (!doc) return null;
  if (!doc._selection) doc._selection = new Selection(doc);
  return doc._selection;
}
globalThis.getSelection = _markNative(function getSelection() {
  return _selectionFor(globalThis.document);
});

globalThis.CSSStyleSheet = class CSSStyleSheet {
  constructor(options) {
    this.cssRules = [];
    this.ownerRule = null;
    this.disabled = false;
    this._rules = [];
  }
  insertRule(rule, index) {
    const idx = index ?? this._rules.length;
    this._rules.splice(idx, 0, { cssText: rule, type: 1 });
    this.cssRules = this._rules;
    return idx;
  }
  deleteRule(index) {
    this._rules.splice(index, 1);
    this.cssRules = this._rules;
  }
  addRule(selector, style, index) {
    return this.insertRule(selector + '{' + style + '}', index);
  }
  removeRule(index) { this.deleteRule(index); }
  replace(text) {
    this._rules = [{ cssText: text, type: 1 }];
    this.cssRules = this._rules;
    return Promise.resolve(this);
  }
  replaceSync(text) {
    this._rules = [{ cssText: text, type: 1 }];
    this.cssRules = this._rules;
  }
};

Object.defineProperty(Document.prototype, 'adoptedStyleSheets', {
  get() { return this._adoptedStyleSheets || []; },
  set(sheets) { this._adoptedStyleSheets = sheets; },
});

globalThis.__mutationObservers = [];
globalThis.MutationObserver = class MutationObserver {
  constructor(callback) {
    this._callback = callback;
    this._targets = [];
    this._records = [];
  }
  observe(target, options) {
    this._targets.push({ target, options: options || {} });
    globalThis.__mutationObservers.push(this);
  }
  disconnect() {
    this._targets = [];
    const idx = globalThis.__mutationObservers.indexOf(this);
    if (idx >= 0) globalThis.__mutationObservers.splice(idx, 1);
  }
  takeRecords() {
    const r = this._records.slice();
    this._records = [];
    return r;
  }
  _notify(records) {
    this._records.push(...records);
    Promise.resolve().then(() => {
      if (this._records.length > 0) {
        const batch = this._records.splice(0);
        try { this._callback(batch, this); } catch(e) { /* observer errors shouldn't propagate */ }
      }
    });
  }
};
globalThis.__notifyMutation = function(type, target_nid, addedNodes, removedNodes, attributeName, oldValue) {
  if (!globalThis.__mutationObservers.length) return;
  // Use `_wrap` (the canonical node-id → wrapper resolver) instead of a
  // direct cache poke. The previous code referenced `globalThis._cache`,
  // but `_cache` is a module-local Map — the lookup always returned
  // undefined, so the function silently bailed every time. Result: no
  // MutationObserver fired in obscura, ever, despite the call sites being
  // wired up at appendChild / setAttribute. _wrap also lazily creates a
  // wrapper for nodes that didn't have one yet (e.g. children parsed from
  // `set innerHTML`), which we need for record.target/added/removed.
  const target = _wrap(target_nid);
  if (!target) return;
  const record = {
    type: type, // 'childList', 'attributes', 'characterData'
    target: target,
    addedNodes: (addedNodes || []).map(nid => _wrap(nid)).filter(Boolean),
    removedNodes: (removedNodes || []).map(nid => _wrap(nid)).filter(Boolean),
    attributeName: attributeName || null,
    oldValue: oldValue ?? null,
    previousSibling: null,
    nextSibling: null,
  };
  // Walk target → ancestors so a subtree-mode observer rooted at any
  // ancestor matches. The previous implementation just checked that
  // `target.contains` and `target.closest` were defined (always true on
  // any Element), so subtree=true silently behaved like subtree=false and
  // every nested mutation missed its subscriber.
  for (const obs of globalThis.__mutationObservers) {
    let matched = false;
    for (const t of obs._targets) {
      const root = t.target;
      if (!root) continue;
      // Filter by type per the observer options. Default behaviour matches
      // real MutationObserver: attribute mutations need options.attributes,
      // characterData mutations need options.characterData, childList
      // needs options.childList.
      const wantsType =
        (type === 'attributes' && t.options.attributes) ||
        (type === 'characterData' && t.options.characterData) ||
        (type === 'childList' && t.options.childList);
      if (!wantsType) continue;
      if (root._nid === target_nid) { matched = true; break; }
      if (t.options.subtree) {
        // Walk parents until we hit the observed root or run off the tree.
        let cur = target.parentNode;
        while (cur) {
          if (cur._nid === root._nid) { matched = true; break; }
          cur = cur.parentNode;
        }
        if (matched) break;
      }
    }
    if (matched) obs._notify([record]);
  }
};

globalThis.ShadowRoot = class ShadowRoot extends DocumentFragment {};
// Constructible-stylesheet adoption, mirroring Document.adoptedStyleSheets.
Object.defineProperty(globalThis.ShadowRoot.prototype, 'adoptedStyleSheets', {
  get() { return this._adoptedStyleSheets || []; },
  set(sheets) { this._adoptedStyleSheets = sheets; },
  configurable: true,
});
globalThis.__diting_shadowHostNames = new Set(['article','aside','blockquote','body','div','footer','h1','h2','h3','h4','h5','h6','header','main','nav','p','section','span']);
function _isConstructorCE(v) {
  if (typeof v !== 'function') return false;
  try { Reflect.construct(function () {}, [], v); return true; } catch (e) { return false; }
}
const _CE_RESERVED = new Set(['annotation-xml', 'color-profile', 'font-face', 'font-face-src', 'font-face-uri', 'font-face-format', 'font-face-name', 'missing-glyph']);
function _isValidCustomElementName(name) {
  if (typeof name !== 'string' || _CE_RESERVED.has(name)) return false;
  // PotentialCustomElementName (approx): lowercase start, a hyphen, no uppercase.
  return /^[a-z][a-z0-9._·À-￿-]*-[a-z0-9._·À-￿-]*$/.test(name);
}
class CustomElementRegistry {
  constructor() { this._registry = new Map(); this._byCtor = new Map(); this._whenDefinedResolvers = new Map(); this._defining = false; }
  define(name, cls, opts) {
    if (!_isConstructorCE(cls)) throw new TypeError("Failed to execute 'define' on 'CustomElementRegistry': parameter 2 is not a constructor.");
    if (!_isValidCustomElementName(name)) throw new DOMException("Failed to execute 'define' on 'CustomElementRegistry': \"" + name + "\" is not a valid custom element name", "SyntaxError");
    if (this._defining) throw new DOMException("Failed to execute 'define' on 'CustomElementRegistry': operation is not supported while a definition is in progress", "NotSupportedError");
    if (this._registry.has(name)) throw new DOMException("Failed to execute 'define' on 'CustomElementRegistry': the name \"" + name + "\" has already been used with this registry", "NotSupportedError");
    if (this._byCtor.has(cls)) throw new DOMException("Failed to execute 'define' on 'CustomElementRegistry': the constructor has already been used with this registry", "NotSupportedError");
    this._defining = true;
    try { this._byCtor.set(cls, name); this._defineInner(name, cls, opts); } finally { this._defining = false; }
  }
  _defineInner(name, cls, opts) {
    this._registry.set(name, cls);
    // Upgrade existing matching elements: instantiate the class on each,
    // fire connectedCallback if the element is in the document. Without
    // this, lit / MusicKit / Polymer components never wire up their
    // shadow DOM or render, leaving heavy chunks of YouTube,
    // music.apple.com, and any web-component site as empty shells.
    try {
      const matches = globalThis.document?.querySelectorAll(name) || [];
      for (const el of matches) this._upgradeElement(el, cls);
    } catch (e) {}
    const resolvers = this._whenDefinedResolvers.get(name);
    if (resolvers) {
      for (const r of resolvers) r(cls);
      this._whenDefinedResolvers.delete(name);
    }
  }
  _upgradeElement(el, cls) {
    if (el.__customUpgraded) return;
    el.__customUpgraded = true;
    try {
      // Web Components spec: copy own props from the prototype onto the
      // element. JS-side classes define behavior via methods on the
      // prototype; we don't truly swap prototypes (Element is shared),
      // so attach the prototype methods directly to the instance.
      const proto = cls.prototype;
      for (const key of Object.getOwnPropertyNames(proto)) {
        if (key === 'constructor') continue;
        const desc = Object.getOwnPropertyDescriptor(proto, key);
        if (desc) Object.defineProperty(el, key, desc);
      }
      // Run constructor-side init on the element. Real custom elements
      // run the class constructor, but Element instances aren't a `cls`
      // subclass here; calling `.call(el)` runs whatever init logic the
      // class defines without needing a new allocation.
      try { cls.call(el); } catch (e) {}
      if (typeof el.connectedCallback === 'function' && globalThis.document?.contains?.(el)) {
        try { el.connectedCallback(); } catch (e) {}
      }
    } catch (e) {}
  }
  get(name) { return this._registry.get(name); }
  getName(cls) {
    if (!_isConstructorCE(cls)) throw new TypeError("Failed to execute 'getName' on 'CustomElementRegistry': parameter 1 is not a constructor.");
    return this._byCtor.has(cls) ? this._byCtor.get(cls) : null;
  }
  whenDefined(name) {
    if (!_isValidCustomElementName(name)) return Promise.reject(new DOMException("Failed to execute 'whenDefined' on 'CustomElementRegistry': \"" + name + "\" is not a valid custom element name", "SyntaxError"));
    const cls = this._registry.get(name);
    if (cls) return Promise.resolve(cls);
    return new Promise((resolve) => {
      const list = this._whenDefinedResolvers.get(name) || [];
      list.push(resolve);
      this._whenDefinedResolvers.set(name, list);
    });
  }
  upgrade(root) {
    if (!root || !root.querySelectorAll) return;
    for (const [name, cls] of this._registry.entries()) {
      const matches = root.querySelectorAll(name);
      for (const el of matches) this._upgradeElement(el, cls);
    }
  }
}
globalThis.CustomElementRegistry = CustomElementRegistry;
globalThis.customElements = new CustomElementRegistry();
globalThis.HTMLUnknownElement = Element;
// ElementInternals: form-associated custom element internals. Validity/state
// are JS-observable; ARIA reflection that needs the accessibility tree is not.
globalThis.ElementInternals = class ElementInternals {
  constructor(el) { this._el = el; this._valid = true; this._flags = {}; this._message = ''; this._value = null; this._states = new Set(); }
  setFormValue(value, state) { this._value = value; }
  setValidity(flags, message, anchor) {
    flags = flags || {};
    const bad = Object.keys(flags).some((k) => k !== 'valid' && flags[k]);
    if (bad && (message == null || message === '')) throw new TypeError("Failed to execute 'setValidity' on 'ElementInternals': The second argument should not be empty if one or more flags in the first argument are true.");
    this._flags = flags; this._valid = !bad; this._message = bad ? String(message) : '';
  }
  checkValidity() { return this._valid; }
  reportValidity() { return this._valid; }
  get validity() {
    const f = this._flags || {};
    return { valid: this._valid, valueMissing: !!f.valueMissing, typeMismatch: !!f.typeMismatch, patternMismatch: !!f.patternMismatch, tooLong: !!f.tooLong, tooShort: !!f.tooShort, rangeUnderflow: !!f.rangeUnderflow, rangeOverflow: !!f.rangeOverflow, stepMismatch: !!f.stepMismatch, badInput: !!f.badInput, customError: !!f.customError };
  }
  get validationMessage() { return this._message || ''; }
  get willValidate() { return true; }
  get form() { return this._el && this._el.closest ? this._el.closest('form') : null; }
  get labels() { return _nodeList([]); }
  get shadowRoot() { return (this._el && this._el._shadowRoot) || null; }
  get states() { return this._states; }
};
// Full standard constant set (issue #439). The partial version here lacked
// FILTER_ACCEPT/REJECT/SKIP and most SHOW_* values, so the canonical
// `acceptNode() { return NodeFilter.FILTER_ACCEPT; }` filter idiom returned
// undefined and TreeWalker/NodeIterator rejected every node.
globalThis.NodeFilter = {
  SHOW_ALL: 0xFFFFFFFF,
  SHOW_ELEMENT: 0x1,
  SHOW_ATTRIBUTE: 0x2,
  SHOW_TEXT: 0x4,
  SHOW_CDATA_SECTION: 0x8,
  SHOW_ENTITY_REFERENCE: 0x10,
  SHOW_ENTITY: 0x20,
  SHOW_PROCESSING_INSTRUCTION: 0x40,
  SHOW_COMMENT: 0x80,
  SHOW_DOCUMENT: 0x100,
  SHOW_DOCUMENT_TYPE: 0x200,
  SHOW_DOCUMENT_FRAGMENT: 0x400,
  SHOW_NOTATION: 0x800,
  FILTER_ACCEPT: 1,
  FILTER_REJECT: 2,
  FILTER_SKIP: 3,
};
// ResizeObserver is defined earlier with real per-target firing; the stub
// that previously lived here was a no-op that clobbered the real class.
//
// IntersectionObserver: without a layout engine we can't compute real
// intersection geometry, so every observed target is treated as fully
// in-viewport (`isIntersecting: true`, `intersectionRatio: 1`). Real
// libraries lean on this in three patterns we must support:
//
//   1. Lazy load: observe(img) -> first intersection -> load src -> unobserve.
//      One fire is enough — covered by the initial microtask fire.
//   2. Infinite scroll: observe(sentinel) -> on intersection load more ->
//      new sentinel mounts -> fire again. Needs re-fires as DOM grows.
//   3. Reveal-on-scroll animations: observe(card) -> isIntersecting flips
//      true once and an animation runs. One fire is enough.
//
// To cover (2) without spinning forever, we burst-fire at an exponential
// backoff schedule and ALSO re-fire whenever the DOM mutates (a strong
// signal that the page just rendered something new). Per-observer total
// fire cap stops us from looping on a never-disconnected observer.
globalThis.__intersectionObservers = [];
globalThis.IntersectionObserver = class IntersectionObserver {
  constructor(callback, options) {
    this._callback = callback;
    this._options = options || {};
    this._targets = new Set();
    this._connected = true;
    this._fireCount = 0;
    globalThis.__intersectionObservers.push(this);
  }
  _fireFor(targets) {
    if (!this._connected || !targets.length || this._fireCount >= 256) return;
    this._fireCount++;
    const records = targets.map(target => ({
      target,
      isIntersecting: true,
      intersectionRatio: 1,
      boundingClientRect: target.getBoundingClientRect
        ? target.getBoundingClientRect()
        : { x: 0, y: 0, width: 100, height: 20, top: 0, left: 0, right: 100, bottom: 20 },
      intersectionRect: target.getBoundingClientRect
        ? target.getBoundingClientRect()
        : { x: 0, y: 0, width: 100, height: 20, top: 0, left: 0, right: 100, bottom: 20 },
      rootBounds: { x: 0, y: 0, width: 1280, height: 720, top: 0, left: 0, right: 1280, bottom: 720 },
      time: Date.now(),
    }));
    try { this._callback(records, this); } catch (e) { /* IO callbacks must not propagate */ }
  }
  observe(el) {
    if (!el || !this._connected) return;
    if (this._targets.has(el)) return;
    this._targets.add(el);
    Promise.resolve().then(() => this._fireFor([el]));
    // Exponential burst to cover infinite-scroll sentinels that "re-arm"
    // after content lands. Without a real scroll/layout signal, we fake the
    // re-fire schedule. Beyond ~10s the page has usually settled.
    [120, 500, 1500, 3500, 7000].forEach(delay => {
      setTimeout(() => {
        if (this._connected && this._targets.has(el)) this._fireFor([el]);
      }, delay);
    });
  }
  unobserve(el) { this._targets.delete(el); }
  disconnect() {
    this._connected = false;
    this._targets.clear();
    const idx = globalThis.__intersectionObservers.indexOf(this);
    if (idx >= 0) globalThis.__intersectionObservers.splice(idx, 1);
  }
  takeRecords() { return []; }
  get root() { return this._options.root || null; }
  get rootMargin() { return this._options.rootMargin || "0px 0px 0px 0px"; }
  get thresholds() {
    const t = this._options.threshold;
    if (t == null) return [0];
    return Array.isArray(t) ? t.slice() : [t];
  }
};
// When the DOM mutates (e.g. infinite scroll loads a batch of items), re-fire
// every active IntersectionObserver so libraries observing dynamic content
// see a fresh isIntersecting=true event. Uses the same per-observer fire cap
// to prevent runaway loops if the page is mutating in a tight cycle.
(function() {
  const reFire = () => {
    for (const obs of globalThis.__intersectionObservers) {
      if (!obs._connected) continue;
      const ts = [...obs._targets];
      if (ts.length) obs._fireFor(ts);
    }
  };
  // Lazy-attach a single MutationObserver on document.body once the page is
  // ready, debounced via a microtask so a flurry of mutations only triggers
  // one IO sweep.
  let pending = false;
  const wireUp = () => {
    if (!globalThis.document?.body) return;
    const mo = new MutationObserver(() => {
      if (pending) return;
      pending = true;
      Promise.resolve().then(() => { pending = false; reFire(); });
    });
    try { mo.observe(globalThis.document.body, {childList: true, subtree: true}); } catch {}
  };
  if (globalThis.document?.body) wireUp();
  else Promise.resolve().then(wireUp);
})();
globalThis.IntersectionObserverEntry = class IntersectionObserverEntry {};
globalThis.PerformanceObserver = class { constructor(){} observe(){} disconnect(){} };

globalThis.DOMException = (function () {
  const NAME_TO_CODE = {
    IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
    InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
    NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
    SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
    InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18,
    NetworkError: 19, AbortError: 20, URLMismatchError: 21,
    QuotaExceededError: 22, TimeoutError: 23, InvalidNodeTypeError: 24,
    DataCloneError: 25,
  };
  class DOMException extends Error {
    constructor(message = "", name = "Error") {
      super(message);
      this.name = name;
      this.message = String(message);
    }
    get code() { return NAME_TO_CODE[this.name] || 0; }
  }
  const CONSTS = {
    INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
    WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
    NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
    INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
    INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
    VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18,
    NETWORK_ERR: 19, ABORT_ERR: 20, URL_MISMATCH_ERR: 21,
    QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23, INVALID_NODE_TYPE_ERR: 24,
    DATA_CLONE_ERR: 25,
  };
  for (const k in CONSTS) {
    Object.defineProperty(DOMException, k, { value: CONSTS[k], enumerable: true });
    Object.defineProperty(DOMException.prototype, k, { value: CONSTS[k], enumerable: true });
  }
  return DOMException;
})();
globalThis.Event = class Event {
  // WebIDL: type is a required argument and is coerced to a string — Chrome
  // throws "1 argument required, but only 0 present." and `new Event(123).type`
  // is "123", not the number (issue #552). Subclasses inherit both via super.
  constructor(t,o={}) { if (arguments.length < 1) throw new TypeError("Failed to construct 'Event': 1 argument required, but only 0 present."); this.type=String(t);this.bubbles=!!o.bubbles;this.cancelable=!!o.cancelable;this.composed=!!o.composed;this.defaultPrevented=false;this.target=null;this.currentTarget=null;this.eventPhase=0;this.timeStamp=Date.now();this._propagationStopped=false;this._immediatePropagationStopped=false; }
  get isTrusted() { return true; }
  preventDefault() { if (this.cancelable) this.defaultPrevented=true; } stopPropagation(){ this._propagationStopped=true; } stopImmediatePropagation(){ this._propagationStopped=true; this._immediatePropagationStopped=true; }
  initEvent(type,bubbles,cancelable) { if (arguments.length < 1) throw new TypeError("Failed to execute 'initEvent' on 'Event': 1 argument required, but only 0 present."); this.type=String(type);this.bubbles=!!bubbles;this.cancelable=!!cancelable;this.defaultPrevented=false;this._propagationStopped=false;this._immediatePropagationStopped=false; }
  composedPath() {
    if (!this.target) return [];
    const path = [];
    let n = this.target;
    while (n) { path.push(n); n = n.parentNode || null; }
    if (typeof window !== "undefined" && window && path[path.length - 1] !== window) path.push(window);
    return path;
  }
};
globalThis.CustomEvent = class extends Event {
  // WebIDL CustomEventInit: `any detail = null` — undefined detail must read
  // back as null, not undefined (issue #552).
  constructor(t,o={}) { if (arguments.length < 1) throw new TypeError("Failed to construct 'CustomEvent': 1 argument required, but only 0 present."); super(t,o);this.detail=o.detail!==undefined?o.detail:null; }
  // Legacy DOM Level 2 init; some libraries (Starbucks China bundle, older
  // analytics shims) still call createEvent('CustomEvent') + initCustomEvent
  // instead of new CustomEvent(...). See issue #41.
  initCustomEvent(type,bubbles,cancelable,detail) {
    this.type = type;
    this.bubbles = !!bubbles;
    this.cancelable = !!cancelable;
    this.detail = detail;
  }
};
globalThis.MouseEvent = class extends Event {
  constructor(t,o={}) { super(t,o);this.view=o.view||null;this.detail=o.detail||0;this.screenX=o.screenX||0;this.screenY=o.screenY||0;this.clientX=o.clientX||0;this.clientY=o.clientY||0;this.ctrlKey=!!o.ctrlKey;this.altKey=!!o.altKey;this.shiftKey=!!o.shiftKey;this.metaKey=!!o.metaKey;this.button=o.button||0;this.buttons=o.buttons||0;this.relatedTarget=o.relatedTarget||null; }
  // Legacy DOM Level 2 initializer. Positional signature per UI Events spec.
  initMouseEvent(type,canBubble,cancelable,view,detail,screenX,screenY,clientX,clientY,ctrlKey,altKey,shiftKey,metaKey,button,relatedTarget) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'initMouseEvent' on 'MouseEvent': 1 argument required, but only 0 present.");
    this.initEvent(type,canBubble,cancelable);
    this.view=view===undefined?null:view;
    this.detail=detail||0;
    this.screenX=screenX||0;
    this.screenY=screenY||0;
    this.clientX=clientX||0;
    this.clientY=clientY||0;
    this.ctrlKey=!!ctrlKey;
    this.altKey=!!altKey;
    this.shiftKey=!!shiftKey;
    this.metaKey=!!metaKey;
    this.button=button||0;
    this.relatedTarget=relatedTarget===undefined?null:relatedTarget;
  }
};
globalThis.KeyboardEvent = class extends Event {
  constructor(t,o={}) { super(t,o);this.view=o.view||null;this.detail=o.detail||0;this.key=o.key||"";this.code=o.code||"";this.location=o.location||0;this.ctrlKey=!!o.ctrlKey;this.altKey=!!o.altKey;this.shiftKey=!!o.shiftKey;this.metaKey=!!o.metaKey;this.repeat=!!o.repeat; }
  // Legacy DOM Level 3 initializer. Positional signature per the WebKit/Gecko form.
  initKeyboardEvent(type,canBubble,cancelable,view,key,location,ctrlKey,altKey,shiftKey,metaKey) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'initKeyboardEvent' on 'KeyboardEvent': 1 argument required, but only 0 present.");
    this.initEvent(type,canBubble,cancelable);
    this.view=view===undefined?null:view;
    this.key=key===undefined?"":String(key);
    this.location=location||0;
    this.ctrlKey=!!ctrlKey;
    this.altKey=!!altKey;
    this.shiftKey=!!shiftKey;
    this.metaKey=!!metaKey;
  }
};
globalThis.FocusEvent = class extends Event { constructor(t,o={}) { super(t,o);this.relatedTarget=o.relatedTarget||null; } };
globalThis.InputEvent = class extends Event { constructor(t,o={}) { super(t,o);this.data=o.data||null;this.inputType=o.inputType||""; } };
globalThis.ErrorEvent = class extends Event { constructor(t,o={}) { super(t,o);this.message=o.message||"";this.error=o.error||null; } };
globalThis.PointerEvent = class extends Event { constructor(t,o={}) { super(t,o); } };
globalThis.AnimationEvent = class extends Event {};
globalThis.TransitionEvent = class extends Event {};
globalThis.UIEvent = class extends Event {
  constructor(t,o={}) { super(t,o);this.view=o.view||null;this.detail=o.detail||0; }
  // Legacy DOM Level 2 initializer. Positional signature per UI Events spec.
  initUIEvent(type,canBubble,cancelable,view,detail) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'initUIEvent' on 'UIEvent': 1 argument required, but only 0 present.");
    this.initEvent(type,canBubble,cancelable);
    this.view=view===undefined?null:view;
    this.detail=detail||0;
  }
};
globalThis.WheelEvent = class extends Event { constructor(t,o={}) { super(t,o);this.deltaX=o.deltaX||0;this.deltaY=o.deltaY||0;this.deltaZ=o.deltaZ||0;this.deltaMode=o.deltaMode||0; } };

globalThis.CompositionEvent = class extends Event {
  constructor(t,o={}) { super(t,o);this.view=o.view||null;this.detail=o.detail||0;this.data=o.data||""; }
  // Legacy DOM Level 3 initializer. Positional signature per UI Events spec.
  initCompositionEvent(type,canBubble,cancelable,view,data) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'initCompositionEvent' on 'CompositionEvent': 1 argument required, but only 0 present.");
    this.initEvent(type,canBubble,cancelable);
    this.view=view===undefined?null:view;
    this.data=data===undefined?"":String(data);
  }
};
globalThis.PopStateEvent = class extends Event {
  constructor(type, init) {
    super(type, init || {});
    // Real PopStateEvent exposes `state` from the entry being navigated to.
    // The earlier stub inherited Event but never stored state, so
    // `popstate.state` was always undefined and SPA routers reading
    // `event.state` to restore route info would mis-render.
    this.state = init && 'state' in init ? init.state : null;
  }
};
globalThis.HashChangeEvent = class extends Event {};
globalThis.MessageEvent = class extends Event { constructor(t,o={}) { super(t,o);this.data=o.data; } };
globalThis.ProgressEvent = class ProgressEvent extends Event {
  constructor(type, init) {
    super(type, init || {});
    const i = init || {};
    this.lengthComputable = !!i.lengthComputable;
    this.loaded = i.loaded != null ? Number(i.loaded) : 0;
    this.total = i.total != null ? Number(i.total) : 0;
  }
};
globalThis.ClipboardEvent = class extends Event {};
globalThis.SubmitEvent = class extends Event {};

// ToggleEvent backs the popover beforetoggle/toggle events. oldState and
// newState are "open"/"closed". These events do not bubble; beforetoggle is
// cancelable only for the closed -> open (show) transition, toggle is never
// cancelable. See HTML "popover" and html/semantics/popovers WPT.
globalThis.ToggleEvent = class ToggleEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.oldState = init.oldState !== undefined ? String(init.oldState) : "";
    this.newState = init.newState !== undefined ? String(init.newState) : "";
  }
};
_markNative(globalThis.ToggleEvent);

// Missing PromiseRejectionEvent made core-js misdetect the environment and
// override native Promise with a broken polyfill, breaking Vue rendering
// (upstream #521). StorageEvent completes the storage API surface (#522).
// PromiseRejectionEvent's `promise` member is required — Chrome throws
// TypeError when the init dict lacks it.
globalThis.PromiseRejectionEvent = class PromiseRejectionEvent extends Event {
  constructor(type, init) {
    if (arguments.length < 2 || init == null || !('promise' in Object(init))) {
      throw new TypeError(
        "Failed to construct 'PromiseRejectionEvent': required member promise is undefined."
      );
    }
    super(type, init);
    this.promise = init.promise;
    this.reason = init.reason;
  }
};
_markNative(globalThis.PromiseRejectionEvent);

globalThis.StorageEvent = class StorageEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.key = init.key !== undefined ? init.key : null;
    this.oldValue = init.oldValue !== undefined ? init.oldValue : null;
    this.newValue = init.newValue !== undefined ? init.newValue : null;
    this.url = init.url || "";
    this.storageArea = init.storageArea || null;
  }
  initStorageEvent(type, bubbles, cancelable, key, oldValue, newValue, url, storageArea) {
    this.initEvent(type, bubbles, cancelable);
    this.key = key !== undefined ? key : null;
    this.oldValue = oldValue !== undefined ? oldValue : null;
    this.newValue = newValue !== undefined ? newValue : null;
    this.url = url || "";
    this.storageArea = storageArea || null;
  }
};
_markNative(globalThis.StorageEvent);

globalThis.AbortController = class AbortController { constructor(){this.signal={aborted:false,addEventListener(){},removeEventListener(){},onabort:null};} abort(){this.signal.aborted=true;} };
globalThis.AbortSignal = { timeout(ms){return {aborted:false,addEventListener(){},removeEventListener(){}}; } };
// Normalize one Blob part to bytes. `native` newline normalization applies to
// string parts when the Blob/File `endings` option is "native".
function _blobPartToBytes(p, native) {
  if (p == null) return new Uint8Array(0);
  if (typeof Blob === "function" && p instanceof Blob) return p._bytes || new Uint8Array(0);
  if (p instanceof ArrayBuffer) return new Uint8Array(p.slice(0));
  if (ArrayBuffer.isView(p)) return new Uint8Array(p.buffer.slice(p.byteOffset, p.byteOffset + p.byteLength));
  let s = String(p);
  if (native) s = s.replace(/\r\n|\r|\n/g, "\n");
  return new TextEncoder().encode(s);
}
function _bytesToBinaryString(bytes) { let s = ""; for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]); return s; }
if (typeof Blob === "undefined") globalThis.Blob = class Blob {
  constructor(parts, opts) {
    opts = opts || {};
    const endings = opts.endings != null ? String(opts.endings) : "transparent";
    if (endings !== "transparent" && endings !== "native") throw new TypeError("Failed to construct 'Blob': The provided value '" + endings + "' is not a valid enum value of type EndingType.");
    const native = endings === "native";
    const chunks = []; let total = 0;
    if (parts != null) {
      if (typeof parts === "string" || typeof parts[Symbol.iterator] !== "function") throw new TypeError("Failed to construct 'Blob': The provided value cannot be converted to a sequence.");
      for (const p of parts) { const b = _blobPartToBytes(p, native); chunks.push(b); total += b.length; }
    }
    const data = new Uint8Array(total); let off = 0;
    for (const c of chunks) { data.set(c, off); off += c.length; }
    this._bytes = data;
    this.size = total;
    const t = opts.type != null ? String(opts.type) : "";
    this.type = /^[\x20-\x7e]*$/.test(t) ? t.toLowerCase() : "";
  }
  get [Symbol.toStringTag]() { return "Blob"; }
  slice(start, end, contentType) {
    const len = this.size;
    const s = start === undefined ? 0 : (start < 0 ? Math.max(len + start, 0) : Math.min(start, len));
    let e = end === undefined ? len : (end < 0 ? Math.max(len + end, 0) : Math.min(end, len));
    if (e < s) e = s;
    const out = new Blob([], contentType != null ? { type: contentType } : {});
    out._bytes = this._bytes.slice(s, e);
    out.size = out._bytes.length;
    return out;
  }
  text() { return Promise.resolve(new TextDecoder().decode(this._bytes)); }
  arrayBuffer() { return Promise.resolve(_arrayBufferFromBytes(this._bytes)); }
  bytes() { return Promise.resolve(this._bytes.slice()); }
};
if (typeof File === "undefined") globalThis.File = class File extends Blob {
  constructor(parts, name, opts) {
    if (arguments.length < 2) throw new TypeError("Failed to construct 'File': 2 arguments required, but only " + arguments.length + " present.");
    opts = opts || {};
    super(parts, opts);
    this.name = String(name);
    this.lastModified = opts.lastModified != null ? Number(opts.lastModified) : Date.now();
  }
  get [Symbol.toStringTag]() { return "File"; }
};
if (typeof FormData === "undefined") globalThis.FormData = class FormData {
  // https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#constructing-form-data-set
  constructor(form, submitter) {
    this._d = [];
    if (!form) return;
    const els = form.elements || [];
    for (let i = 0; i < els.length; i++) {
      const el = els[i];
      if (!el || !el.name || el.disabled) continue;
      const tag = (el.tagName || "").toUpperCase();
      if (tag === "FIELDSET" || tag === "OBJECT") continue;
      const t = String(el.type || "").toLowerCase();
      if (t === "submit" || t === "button" || t === "reset" || t === "image") continue;
      if ((t === "checkbox" || t === "radio") && !el.checked) continue;
      if (tag === "SELECT" && el.selectedOptions) {
        let any = false;
        for (const opt of el.selectedOptions) { any = true; this._d.push([el.name, String(opt.value ?? "")]); }
        if (any) continue;
      }
      this._d.push([el.name, el.value != null ? String(el.value) : ""]);
    }
    // The submitter's name/value is included (it is the clicked button).
    if (submitter && submitter.name) this._d.push([submitter.name, String(submitter.value ?? "")]);
  }
  // Blob values stay objects (spec: append(name, blobValue, filename));
  // everything else converts to USVString. Stringifying a File here used to
  // turn uploads into the literal "[object File]" on the wire.
  append(k, v, filename) {
    k = String(k);
    if (v != null && typeof v === "object" && typeof Blob === "function" && v instanceof Blob) {
      if (filename !== undefined) v = new File([v], String(filename), { type: v.type });
      this._d.push([k, v]);
    } else {
      this._d.push([k, String(v)]);
    }
  }
  set(k, v, filename) {
    k = String(k);
    if (v != null && typeof v === "object" && typeof Blob === "function" && v instanceof Blob) {
      if (filename !== undefined) v = new File([v], String(filename), { type: v.type });
    } else {
      v = String(v);
    }
    const i = this._d.findIndex(([a]) => a === k);
    if (i >= 0) this._d[i] = [k, v]; else this._d.push([k, v]);
  }
  delete(k){this._d=this._d.filter(([a])=>a!==k);}
  get(k){const e=this._d.find(([a])=>a===k);return e?e[1]:null;}
  getAll(k){return this._d.filter(([a])=>a===k).map(([,v])=>v);}
  has(k){return this._d.some(([a])=>a===k);}
  *entries(){for(let i=0;i<this._d.length;i++)yield this._d[i];}
  *keys(){for(const [k] of this._d)yield k;}
  *values(){for(const [,v] of this._d)yield v;}
  [Symbol.iterator](){return this.entries();}
  forEach(cb,thisArg){this._d.forEach(([k,v])=>cb.call(thisArg,v,k,this));}
};
// application/x-www-form-urlencoded serializer: like encodeURIComponent but
// space -> '+' and also percent-encoding the chars encodeURIComponent leaves
// bare ( ! ~ ' ( ) ), keeping the form-urlencoded safe set ( * - . _ ).
function _formEncode(s){
  return encodeURIComponent(String(s)).replace(/%20/g,'+').replace(/[!'()~]/g, c => '%' + c.charCodeAt(0).toString(16).toUpperCase());
}
function _hexv(c){ if(c>=48&&c<=57)return c-48; if(c>=65&&c<=70)return c-55; if(c>=97&&c<=102)return c-87; return -1; }
if (typeof URLSearchParams === "undefined") globalThis.URLSearchParams = class URLSearchParams {
  constructor(init=""){
    this._p=[];
    this._url=null; // set by URL.searchParams so mutations write back to the URL
    if (typeof URLSearchParams === 'function' && init instanceof URLSearchParams) {
      this._p = init._p.map(pair => [pair[0], pair[1]]);
    } else if(typeof init==="string"){
      this._parseString(init);
    } else if (init && typeof init[Symbol.iterator] === 'function') {
      for (const pair of init) {
        const a = Array.from(pair);
        if (a.length !== 2) throw new TypeError("Failed to construct 'URLSearchParams': Each query pair must be an iterable [name, value] tuple");
        this._p.push([String(a[0]), String(a[1])]);
      }
    } else if (init && typeof init === 'object') {
      Object.keys(init).forEach(k => this._p.push([String(k), String(init[k])]));
    }
  }
  _decode(s){
    // application/x-www-form-urlencoded percent-decoding: decode each valid %XX
    // byte, leave invalid escapes literal (decodeURIComponent throws on the whole
    // string instead), '+' -> space, then UTF-8 decode the resulting bytes.
    s = String(s);
    const out = [];
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (c === 0x2B) { out.push(0x20); }
      else if (c === 0x25 && i + 2 < s.length) {
        const a = _hexv(s.charCodeAt(i + 1)), b = _hexv(s.charCodeAt(i + 2));
        if (a >= 0 && b >= 0) { out.push(a * 16 + b); i += 2; } else { out.push(c); }
      } else if (c < 0x80) { out.push(c); }
      else { const e = new TextEncoder().encode(s[i]); for (let j = 0; j < e.length; j++) out.push(e[j]); }
    }
    try { return new TextDecoder().decode(new Uint8Array(out)); } catch (e) { return s; }
  }
  _parseString(s){
    s = String(s).replace(/^\?/, "");
    if (s === "") return;
    for (const pair of s.split("&")) {
      if (pair === "") continue;
      const i = pair.indexOf("=");
      const k = i === -1 ? pair : pair.slice(0, i);
      const v = i === -1 ? "" : pair.slice(i + 1);
      this._p.push([this._decode(k), this._decode(v)]);
    }
  }
  _setFromString(s){ this._p = []; this._parseString(s); }
  _notify(){ if (this._url) this._url._updateSearch(this.toString()); }
  append(k,v){ this._p.push([String(k),String(v)]); this._notify(); }
  get(k){k=String(k); const p=this._p.find(([key])=>key===k); return p?p[1]:null;}
  getAll(k){k=String(k); return this._p.filter(([key])=>key===k).map(pair=>pair[1]);}
  set(k,v){k=String(k); v=String(v); let done=false; const out=[]; for (const pair of this._p){ if(pair[0]===k){ if(!done){ out.push([k,v]); done=true; } } else out.push(pair); } if(!done) out.push([k,v]); this._p=out; this._notify(); }
  delete(k,v){k=String(k); const hv=(v!==undefined); v=String(v); this._p=this._p.filter(([key,val])=> hv ? !(key===k&&val===v) : key!==k); this._notify();}
  has(k,v){k=String(k); const hv=(v!==undefined); v=String(v); return this._p.some(([key,val])=> hv ? (key===k&&val===v) : key===k);}
  sort(){ this._p.sort((a,b)=> a[0]<b[0]?-1:(a[0]>b[0]?1:0)); this._notify(); }
  get size(){ return this._p.length; }
  toString(){return this._p.map(pair=>_formEncode(pair[0])+"="+_formEncode(pair[1])).join("&");}
  forEach(cb,thisArg){this._p.slice().forEach(pair=>cb.call(thisArg,pair[1],pair[0],this));}
  *entries(){ for (const pair of this._p) yield [pair[0],pair[1]]; }
  *keys(){ for (const pair of this._p) yield pair[0]; }
  *values(){ for (const pair of this._p) yield pair[1]; }
  [Symbol.iterator](){ return this.entries(); }
};

// Conservative XML well-formedness check for DOMParser (upstream 53295fa →
// 20c4628 final form). Only clear errors are flagged: tag mismatch, extra
// closing tag, extra root content, unclosed tags. Self-closing tags count as
// complete elements (869f700) and never push the stack.
const _checkXmlWellFormed = (html) => {
  // Strip comments, CDATA sections, processing instructions, and DOCTYPE
  // declarations — they may contain angle brackets.
  const s = html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<!\[CDATA\[[\s\S]*?\]\]>/g, '')
    .replace(/<\?[\s\S]*?\?>/g, '')
    .replace(/<!DOCTYPE\s[^>]*?>/gi, '');

  const stack = [];
  // Match open / close / self-closing tags.
  // Group 1: tag name.  Group 2: optional '/' before '>'.
  const tagRe = /<\/?([a-zA-Z_][\w.\-:]*)(?:\s[^>]*?)?(\/)?>/g;
  let rootFound = false;
  let match;

  while ((match = tagRe.exec(s)) !== null) {
    const fullTag = match[0];
    const tagName = match[1];
    const isClosing = fullTag.startsWith('</');
    const isSelfClosing = match[2] === '/';

    if (isClosing) {
      if (stack.length === 0) {
        return { wellFormed: false, error: 'error on line 1: extra closing tag </' + tagName + '>' };
      }
      const open = stack.pop();
      if (open !== tagName) {
        return { wellFormed: false, error: 'error on line 1: opening and ending tag mismatch: ' + open + ' and ' + tagName };
      }
      if (stack.length === 0) rootFound = true;
    } else {
      // Opening or self-closing tag. Check for extra content after root.
      if (stack.length === 0 && rootFound) {
        return { wellFormed: false, error: 'error on line 1: extra content after root element' };
      }
      if (isSelfClosing) {
        // Self-closing: complete element, mark rootFound if at root level.
        if (stack.length === 0) rootFound = true;
      } else {
        stack.push(tagName);
      }
    }
  }

  if (stack.length > 0) {
    return { wellFormed: false, error: 'error on line 1: unclosed tag <' + stack[stack.length - 1] + '>' };
  }

  return { wellFormed: true };
};

// Stricter hand-rolled state machine: quote-aware '>' scanning, unterminated
// tags, and exactly one fully closed root element. Catches cases the regex
// pass above cannot (e.g. '>' inside attribute values, zero/multiple roots).
function _xmlWellFormed(src) {
  const s = String(src);
  const stack = [];
  let rootsClosed = 0; // top-level elements fully closed (or self-closed)
  let i = 0;
  const n = s.length;
  while (i < n) {
    const lt = s.indexOf('<', i);
    if (lt === -1) break;
    i = lt;
    if (s.startsWith('<!--', i)) { const e = s.indexOf('-->', i + 4); if (e === -1) return false; i = e + 3; continue; }
    if (s.startsWith('<![CDATA[', i)) { const e = s.indexOf(']]>', i + 9); if (e === -1) return false; i = e + 3; continue; }
    if (s.startsWith('<?', i)) { const e = s.indexOf('?>', i + 2); if (e === -1) return false; i = e + 2; continue; }
    if (s.startsWith('<!', i)) { const e = s.indexOf('>', i + 2); if (e === -1) return false; i = e + 1; continue; }
    // A start/end/self-closing tag: find its '>' while skipping quoted regions.
    let j = i + 1, quote = null;
    while (j < n) {
      const c = s[j];
      if (quote) { if (c === quote) quote = null; }
      else if (c === '"' || c === "'") quote = c;
      else if (c === '>') break;
      j++;
    }
    if (j >= n) return false; // unterminated tag
    const inner = s.slice(i + 1, j).trim();
    i = j + 1;
    if (!inner) return false;
    if (inner[0] === '/') {
      const name = inner.slice(1).trim().split(/\s/)[0];
      if (stack.length === 0 || stack[stack.length - 1] !== name) return false;
      stack.pop();
      if (stack.length === 0) rootsClosed++;
    } else if (inner[inner.length - 1] === '/') {
      if (stack.length === 0) rootsClosed++;
    } else {
      const name = inner.split(/\s/)[0];
      if (!name) return false;
      stack.push(name);
    }
  }
  return stack.length === 0 && rootsClosed === 1;
}

// Real-enough DOMParser. The previous one-liner returned `globalThis.document`,
// so anything that did `new DOMParser().parseFromString(s, 'text/html')` and
// then read `.body.innerHTML` mutated the LIVE page (jQuery 3.x's selector
// feature-detect writes `<form></form>` and wiped real bodies). We parse the
// input into a detached `<html>` element and wrap it so the common Document
// API surface (body / head / documentElement / querySelector* / getElementById /
// getElementsByTagName / getElementsByClassName / title / cloneNode) works.
globalThis.DOMParser = class DOMParser {
  parseFromString(source, mimeType) {
    const html = String(source ?? "");
    const isXml = typeof mimeType === "string" && /xml/i.test(mimeType);
    const root = document.createElement("html");

    // For XML mime types, check well-formedness first (conservative: only
    // clear errors like tag mismatch / extra root are flagged).  If the
    // check fires, build a <parsererror> root so callers doing
    // doc.querySelector('parsererror') get the same signal as in Chrome.
    const xmlError = isXml ? _checkXmlWellFormed(html) : null;
    const isParserError = xmlError && !xmlError.wellFormed;
    if (isParserError) {
      // Build the <parsererror> element directly rather than via innerHTML:
      // the fragment parser routes unknown elements into <head>, which would
      // make firstElementChild (the documentElement) a <head> instead of the
      // parsererror Chrome would hand back.
      const pe = document.createElement('parsererror');
      pe.textContent = xmlError.error;
      root.appendChild(pe);
    } else {
      // innerHTML parses children via html5ever fragment-parsing rules. Most
      // HTML inputs start with `<!DOCTYPE>` / `<html>` / `<head>` etc.; the
      // fragment parser strips the outer `<html>` and emits its head+body
      // children, which is what callers want.
      try { root.innerHTML = html; } catch (e) { /* leave empty on parse error */ }
    }

    // For XML mime types, surface a <parsererror> on clearly-malformed input so
    // error-detection code (doc.querySelector('parsererror')) works, matching
    // Chrome. We have no XML parser, so the tree stays HTML-parsed.
    if (isXml && !_xmlWellFormed(html)) {
      try {
        root.innerHTML = '';
        const pe = document.createElement('parsererror');
        pe.setAttribute('xmlns', 'http://www.w3.org/1999/xhtml');
        pe.innerHTML = 'This page contains the following errors:<div>error while parsing XML</div>';
        root.appendChild(pe);
      } catch (e) { /* ignore */ }
    }

    // Helper: depth-first walk to find an element by predicate.
    const walk = (node, pred) => {
      if (!node) return null;
      if (node.nodeType === 1 && pred(node)) return node;
      const children = node.children || [];
      for (let i = 0; i < children.length; i++) {
        const r = walk(children[i], pred);
        if (r) return r;
      }
      return null;
    };

    const findByTagName = (name) => walk(root, n => n.tagName === name);

    const docNode = {
      _root: root,
      nodeName: "#document",
      nodeType: 9,
      contentType: isXml ? (mimeType || "application/xml") : "text/html",
      get documentElement() {
        // For XML parsererror docs, return the <parsererror> child, not the
        // <html> wrapper — matches Chrome's behavior.
        if (isParserError) return root.firstElementChild;
        return root;
      },
      get body() { return findByTagName("BODY"); },
      get head() { return findByTagName("HEAD"); },
      get title() {
        const t = findByTagName("TITLE");
        return t ? (t.textContent || "").replace(/[\t\n\f\r ]+/g, " ").trim() : "";
      },
      set title(value) {
        let t = findByTagName("TITLE");
        if (!t) {
          let head = findByTagName("HEAD");
          if (!head) {
            head = document.createElement("head");
            root.insertBefore(head, findByTagName("BODY"));
          }
          t = document.createElement("title");
          head.appendChild(t);
        }
        t.textContent = String(value);
      },
      get referrer() { return ""; },
      get firstChild() { return root; },
      get lastChild() { return root; },
      get children() { return [root]; },
      get childNodes() { return [root]; },
      // Document metadata the WHATWG interface exposes; DOMParser documents have
      // URL about:blank, are already fully parsed, and carry no stylesheets.
      get URL() { return "about:blank"; },
      get documentURI() { return "about:blank"; },
      get baseURI() { return "about:blank"; },
      get compatMode() { return "CSS1Compat"; },
      get characterSet() { return "UTF-8"; },
      get charset() { return "UTF-8"; },
      get inputEncoding() { return "UTF-8"; },
      get readyState() { return "complete"; },
      get styleSheets() { return { length: 0, item() { return null; }, [Symbol.iterator]: function* () {} }; },
      get defaultView() { return null; },
      get ownerDocument() { return null; },
      createTreeWalker(r, ws, f) { return document.createTreeWalker(r || root, ws, f); },
      createNodeIterator(r, ws, f) { return document.createNodeIterator(r || root, ws, f); },
      querySelector(s) {
        // For XML parsererror docs, check the root element as well —
        // the <parsererror> is the documentElement, not a descendant.
        return root.querySelector(s) || (isParserError && root.matches(s) ? root : null);
      },
      querySelectorAll(s) { return root.querySelectorAll(s); },
      getElementById(id) {
        return walk(root, n => n.getAttribute && n.getAttribute("id") === id);
      },
      getElementsByTagName(t) {
        return root.querySelectorAll(t);
      },
      getElementsByClassName(c) {
        return _getElementsByClassName(root, c);
      },
      getElementsByName(n) {
        return root.querySelectorAll(`[name="${n}"]`);
      },
      createElement: (t) => document.createElement(t),
      createElementNS: (ns, t) => document.createElement(t),
      createTextNode: (t) => document.createTextNode(t),
      createComment: (t) => document.createComment(t),
      createDocumentFragment: () => document.createDocumentFragment(),
      createRange: () => new Range(),
      createEvent: (type) => document.createEvent(type),
      createCDATASection: (data) => {
        if (mimeType === "text/html") throw new DOMException("createCDATASection is not supported in HTML documents", "NotSupportedError");
        const s = String(data);
        if (s.indexOf("]]>") !== -1) throw new DOMException("CDATA section data must not contain ']]>'", "InvalidCharacterError");
        return new CDATASection(+_dom("create_text_node", s));
      },
      createProcessingInstruction: (target, data) => {
        const t = String(target), s = String(data);
        if (!_isValidPITarget(t)) throw new DOMException("Invalid processing instruction target", "InvalidCharacterError");
        if (s.indexOf("?>") !== -1) throw new DOMException("Processing instruction data must not contain '?>'", "InvalidCharacterError");
        return new ProcessingInstruction(+_dom("create_text_node", s), t);
      },
      adoptNode: (n) => n,
      importNode: (n) => n,
      // Document-level node insertion. Detached docs from createHTMLDocument /
      // createDocument back onto the same tree, so appending lands under the
      // documentElement; enough for dom/common.js to build its Range fixtures.
      appendChild: function (n) { try { root.appendChild(n); } catch (e) {} return n; },
      removeChild: function (n) { try { root.removeChild(n); } catch (e) {} return n; },
      insertBefore: function (n, ref) { try { root.insertBefore(n, ref); } catch (e) {} return n; },
      _docType: null,
      get doctype() { return this._docType; },
      cloneNode: function (deep) {
        return new DOMParser().parseFromString(root.outerHTML, mimeType);
      },
      contains(n) { return root.contains ? root.contains(n) : false; },
      addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
    };
    return docNode;
  }
};
globalThis.XMLSerializer = class XMLSerializer {
  serializeToString(node) {
    if (!node) return "";
    if (node.nodeType === 10) {
      let s = "<!DOCTYPE " + (node.name || "html");
      if (node.publicId) s += ' PUBLIC "' + node.publicId + '"';
      if (node.systemId) {
        if (!node.publicId) s += " SYSTEM";
        s += ' "' + node.systemId + '"';
      }
      s += ">";
      return s;
    }
    if (node.outerHTML !== undefined) return node.outerHTML;
    if (node.nodeType === 9) {
      let s = "";
      if (node.doctype) s += this.serializeToString(node.doctype);
      if (node.documentElement) s += node.documentElement.outerHTML;
      return s;
    }
    if (node.nodeType === 3) return node.textContent || "";
    if (node.nodeType === 8) return "<!--" + (node.textContent || "") + "-->";
    return "";
  }
};
// performance.now(): ms since timeOrigin. Monotonically non-decreasing —
// never return below the last reading (a wall-clock adjustment or NTP step
// would otherwise run it backwards, upstream #497) — but equal readings are
// allowed and no synthetic per-call increment keeps tight loops from running
// the clock ahead of real elapsed time (upstream d93ff51). timeOrigin is read
// dynamically because the constructor sets it to 0 below and __diting_init
// assigns the real navigation timestamp after construction.
// Each navigation rebuilds the whole runtime (page.rs init_js), so the floor
// starts fresh per document like a real browser's per-navigation clock.
var _perfLast = -Infinity;
// PerformanceEntry with prototype accessors (same lie-detector posture as
// FontFace above): name/entryType/startTime/duration on the prototype, backed
// by private slots, all reporting [native code].
class PerformanceEntry {
  constructor(name, entryType, startTime, duration) {
    this._peName = name; this._peType = entryType;
    this._peStart = startTime; this._peDur = duration;
    this._peDetail = null;
  }
  get name() { return this._peName; }
  get entryType() { return this._peType; }
  get startTime() { return this._peStart; }
  get duration() { return this._peDur; }
  toJSON() {
    const o = { name: this._peName, entryType: this._peType,
                startTime: this._peStart, duration: this._peDur };
    if (this._peDetail !== null) o.detail = this._peDetail;
    return o;
  }
}
_markNativeProto(PerformanceEntry.prototype);
class _Performance {
  constructor() {
    this._marks = []; this._measures = [];
    this._navEntry = null; this._paintEntries = null;
    this.timeOrigin = 0;
    this.timing = { navigationStart: 0, domContentLoadedEventEnd: 0, loadEventEnd: 0 };
    this.navigation = { type: 0, redirectCount: 0 };
    this.memory = {
      jsHeapSizeLimit: 2172649472,
      totalJSHeapSize: 19321856,
      usedJSHeapSize: 16781520,
    };
  }
  now() {
    var ms = Date.now() - (this.timeOrigin || 0);
    if (ms < _perfLast) return _perfLast;
    _perfLast = ms;
    return _perfLast;
  }
  // User-timing marks/measures are recorded for real — analytics bundles
  // (Sentry, web-vitals wrappers) read them back and an always-empty buffer
  // pushes them into fallback/error branches. Capped so a mark loop can't
  // grow the buffer unbounded.
  mark(name, options) {
    if (arguments.length === 0) {
      throw new TypeError("Failed to execute 'mark' on 'Performance': 1 argument required, but only 0 present.");
    }
    const o = options || {};
    const t = typeof o.startTime === 'number' && isFinite(o.startTime) ? o.startTime : this.now();
    const e = new PerformanceEntry(String(name), 'mark', t, 0);
    if ('detail' in o) e._peDetail = o.detail;
    this._marks.push(e);
    if (this._marks.length > 500) this._marks.shift();
    return e;
  }
  measure(name, start, end) {
    if (arguments.length === 0) {
      throw new TypeError("Failed to execute 'measure' on 'Performance': 1 argument required, but only 0 present.");
    }
    var startTime = 0, endTime = this.now();
    if (start !== undefined && start !== null) {
      if (typeof start === 'string') {
        const m = this._lastMark(start);
        if (!m) throw new SyntaxError("Failed to execute 'measure' on 'Performance': The mark '" + start + "' does not exist.");
        startTime = m.startTime;
      } else if (typeof start === 'number' && isFinite(start)) {
        startTime = start;
      }
    }
    if (end !== undefined && end !== null) {
      if (typeof end === 'string') {
        const m = this._lastMark(end);
        if (!m) throw new SyntaxError("Failed to execute 'measure' on 'Performance': The mark '" + end + "' does not exist.");
        endTime = m.startTime;
      } else if (typeof end === 'number' && isFinite(end)) {
        endTime = end;
      }
    }
    // Chrome allows a negative duration when end < start; keep that.
    const e = new PerformanceEntry(String(name), 'measure', startTime, endTime - startTime);
    this._measures.push(e);
    if (this._measures.length > 500) this._measures.shift();
    return e;
  }
  _lastMark(name) {
    for (let i = this._marks.length - 1; i >= 0; i--) {
      if (this._marks[i].name === name) return this._marks[i];
    }
    return null;
  }
  // Derived NavigationTiming: one entry, startTime 0, duration = load offset,
  // carrying the Level-2 fields analytics actually read (domContentLoadedEventEnd,
  // loadEventEnd, responseStart, type...). Own data props — Chrome puts these
  // on PerformanceNavigationTiming.prototype, but we don't expose that
  // constructor so detectors can't diff the descriptor; the entry surface
  // itself is what libraries consume.
  _navTiming() {
    if (this._navEntry) return this._navEntry;
    const t = this.timing || {};
    const nav0 = t.navigationStart || this.timeOrigin;
    if (!nav0) return null;
    const rel = (v) => (typeof v === 'number' && v > 0 ? Math.max(0, v - nav0) : 0);
    const loadEnd = t.loadEventEnd || t.navigationStart;
    const e = new PerformanceEntry(
      (globalThis.location && globalThis.location.href) || '',
      'navigation', 0, Math.max(0, loadEnd - nav0));
    const extra = {
      initiatorType: 'navigation', nextHopProtocol: '',
      type: 'navigate', redirectCount: 0,
      unloadEventStart: 0, unloadEventEnd: 0,
      fetchStart: rel(t.fetchStart) || 1,
      domainLookupStart: rel(t.domainLookupStart), domainLookupEnd: rel(t.domainLookupEnd),
      connectStart: rel(t.connectStart), connectEnd: rel(t.connectEnd),
      secureConnectionStart: rel(t.secureConnectionStart),
      requestStart: rel(t.requestStart), responseStart: rel(t.responseStart), responseEnd: rel(t.responseEnd),
      transferSize: 0, encodedBodySize: 0, decodedBodySize: 0,
      domInteractive: rel(t.domInteractive),
      domContentLoadedEventStart: rel(t.domContentLoadedEventStart),
      domContentLoadedEventEnd: rel(t.domContentLoadedEventEnd),
      domComplete: rel(t.domComplete),
      loadEventStart: rel(t.loadEventStart), loadEventEnd: rel(t.loadEventEnd),
    };
    Object.assign(e, extra);
    e.toJSON = function() {
      return Object.assign(PerformanceEntry.prototype.toJSON.call(this), extra);
    };
    _markNative(e.toJSON);
    this._navEntry = e;
    return e;
  }
  // Paint timings derived from the (simulated) DCL offset: FP lands before
  // FCP, both before domContentLoadedEventEnd. Generated once per navigation.
  _paintTimings() {
    if (this._paintEntries) return this._paintEntries;
    const t = this.timing || {};
    const nav0 = t.navigationStart;
    const dcl = (nav0 && t.domContentLoadedEventEnd) ? Math.max(1, t.domContentLoadedEventEnd - nav0) : 300;
    const fp = Math.max(1, Math.floor(dcl * (0.55 + _fpRand(644) * 0.25)));
    const fcp = Math.max(fp + 1, Math.floor(dcl * (0.78 + _fpRand(645) * 0.2)));
    this._paintEntries = [
      new PerformanceEntry('first-paint', 'paint', fp, 0),
      new PerformanceEntry('first-contentful-paint', 'paint', fcp, 0),
    ];
    return this._paintEntries;
  }
  _all() {
    const nav = this._navTiming();
    const list = (nav ? [nav] : []).concat(this._paintTimings(), this._marks, this._measures);
    return list.sort((a, b) => a.startTime - b.startTime);
  }
  getEntries() { return this._all().slice(); }
  getEntriesByName(name, type) {
    name = String(name);
    return this._all().filter(
      (e) => e.name === name && (type === undefined || e.entryType === String(type)));
  }
  getEntriesByType(type) {
    type = String(type);
    if (type === 'navigation') { const n = this._navTiming(); return n ? [n] : []; }
    if (type === 'paint') return this._paintTimings().slice();
    if (type === 'mark') return this._marks.slice();
    if (type === 'measure') return this._measures.slice();
    // 'resource' stays honestly empty: we have no per-request network
    // timings, and a fabricated waterfall would be a lying telemetry
    // surface (worse than absent). Unknown types return [] like Chrome.
    return [];
  }
  clearMarks(name) {
    if (name === undefined) { this._marks = []; return; }
    this._marks = this._marks.filter((e) => e.name !== String(name));
  }
  clearMeasures(name) {
    if (name === undefined) { this._measures = []; return; }
    this._measures = this._measures.filter((e) => e.name !== String(name));
  }
  setResourceTimingBufferSize() {}
  clearResourceTimings() {}
  // Per-navigation cache reset, called from __diting_init after timing lands.
  _resetDerived() { this._navEntry = null; this._paintEntries = null; }
}
_markNativeProto(_Performance.prototype);
globalThis.performance = globalThis.performance || new _Performance();

var _commonFonts = [
  'Arial', 'Arial Black', 'Arial Narrow',
  'Baskerville', 'Book Antiqua',
  'Calibri', 'Cambria', 'Candara', 'Consolas', 'Courier New',
  'DejaVu Sans', 'DejaVu Sans Mono', 'DejaVu Serif',
  'Futura',
  'Garamond', 'Georgia', 'Gill Sans',
  'Helvetica',
  'Impact',
  'Liberation Sans', 'Liberation Sans Mono', 'Liberation Serif',
  'Lucida Console', 'Lucida Handwriting',
  'Microsoft Sans Serif', 'Monaco',
  'Noto Sans', 'Noto Serif',
  'Palatino Linotype',
  'Segoe UI',
  'Tahoma', 'Times New Roman', 'Trebuchet MS',
  'Verdana',
  'Webdings', 'Wingdings',
];
// FontFace constructor. document.fonts below returns FontFace-tagged objects,
// but the WorkOS/font-loader code path calls `new FontFace(...)` directly and
// a missing global throws a ReferenceError that aborts the script bundle —
// intermittently killing the whole React mount (the authk flaky blank page).
class FontFace {
  // Spec-shaped FontFace: `family`/`style`/`weight`/`stretch`/`status` are
  // PROTOTYPE accessors backed by private slots, not instance data
  // properties. Bot detectors (Castle/WorkOS) do
  // `getOwnPropertyDescriptor(FontFace.prototype, 'family')` — instance
  // props made that undefined, which their lie-detector flags, which sends
  // every "isolated iframe" probe into the MAIN document (page wipe).
  constructor(family, source, descriptors) {
    const d = descriptors || {};
    this._ffFamily = String(family == null ? '' : family);
    this._ffSource = String(source == null ? '' : source);
    this._ffStyle = d.style || 'normal';
    this._ffWeight = d.weight || '400';
    this._ffStretch = d.stretch || 'normal';
    this._ffDisplay = d.display || 'auto';
    this._ffUnicodeRange = d.unicodeRange || 'U+0-10FFFF';
    this._ffVariant = d.variant || 'normal';
    this._ffFeatureSettings = d.featureSettings || 'normal';
    this._ffStatus = 'unloaded';
    this._ffLoaded = null;
  }
  get family() { return this._ffFamily; }
  set family(v) { this._ffFamily = String(v == null ? '' : v); }
  get source() { return this._ffSource; }
  set source(v) { this._ffSource = String(v == null ? '' : v); }
  get style() { return this._ffStyle; }
  set style(v) { this._ffStyle = String(v == null ? 'normal' : v); }
  get weight() { return this._ffWeight; }
  set weight(v) { this._ffWeight = String(v == null ? '400' : v); }
  get stretch() { return this._ffStretch; }
  set stretch(v) { this._ffStretch = String(v == null ? 'normal' : v); }
  get display() { return this._ffDisplay; }
  set display(v) { this._ffDisplay = String(v == null ? 'auto' : v); }
  get unicodeRange() { return this._ffUnicodeRange; }
  set unicodeRange(v) { this._ffUnicodeRange = String(v == null ? 'U+0-10FFFF' : v); }
  get variant() { return this._ffVariant; }
  set variant(v) { this._ffVariant = String(v == null ? 'normal' : v); }
  get featureSettings() { return this._ffFeatureSettings; }
  set featureSettings(v) { this._ffFeatureSettings = String(v == null ? 'normal' : v); }
  get status() { return this._ffStatus; }
  get loaded() {
    if (!this._ffLoaded) this._ffLoaded = this.load();
    return this._ffLoaded;
  }
  load() {
    this._ffStatus = 'loaded';
    return Promise.resolve(this);
  }
}
Object.defineProperty(FontFace.prototype, Symbol.toStringTag, { value: 'FontFace', configurable: true });
_markNativeProto(FontFace.prototype);
globalThis.FontFace = FontFace;
_markNative(FontFace);

Object.defineProperty(Document.prototype, 'fonts', {
  get() {
    const _set = _commonFonts.map((name, i) => ({
      family: name, style: 'normal', weight: '400', stretch: 'normal',
      status: 'loaded', loaded: Promise.resolve(this),
      [Symbol.toStringTag]: 'FontFace',
    }));
    _set.forEach = (fn) => { _set.forEach(fn); };
    _set.has = (f) => typeof f === 'string'
      ? _commonFonts.some(n => n.toLowerCase() === f.toLowerCase())
      : _set.some(ff => ff.family === f?.family);
    _set.delete = (f) => false;
    _set.clear = () => {};
    _set.add = () => {};
    _set.load = () => Promise.resolve(_set);
    _set.check = (font) => {
      const m = typeof font === 'string' ? font.match(/["']([^"']+)["']/) : null;
      return m ? _commonFonts.some(n => n.toLowerCase() === m[1].toLowerCase()) : true;
    };
    _set.ready = Promise.resolve(_set);
    _set.status = 'loaded';
    _set.addEventListener = () => {};
    _set.removeEventListener = () => {};
    _set.dispatchEvent = () => true;
    return _set;
  },
  configurable: true,
});
globalThis.crypto = globalThis.crypto || {
  // Fill an integer TypedArray from the OS CSPRNG. Filling the underlying bytes
  // (not per-element Math.random) keeps the distribution uniform across every
  // typed-array width and is actually cryptographically random.
  getRandomValues(arr) {
    if (!ArrayBuffer.isView(arr) || arr instanceof DataView ||
        arr instanceof Float32Array || arr instanceof Float64Array ||
        (typeof Float16Array !== 'undefined' && arr instanceof Float16Array)) {
      throw new DOMException("The provided ArrayBufferView is not an integer-typed array", "TypeMismatchError");
    }
    if (arr.byteLength > 65536) {
      throw new DOMException("The requested length exceeds 65536 bytes", "QuotaExceededError");
    }
    const bytes = _OPS.op_random_bytes(arr.byteLength);
    new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength).set(bytes);
    return arr;
  },
  randomUUID() {
    const b = _OPS.op_random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let s = "";
    for (let i = 0; i < 16; i++) {
      s += (b[i] + 0x100).toString(16).slice(1);
      if (i === 3 || i === 5 || i === 7 || i === 9) s += "-";
    }
    return s;
  },
};
// Real structured clone (not JSON). JSON.parse(JSON.stringify) silently drops
// ArrayBuffer/TypedArray (they serialize to {}), so Cloudflare's turnstile
// orchestrate loses every byte it tries to round-trip through postMessage and
// the challenge never completes (issue #389). Clone buffers, typed arrays,
// maps/sets, dates, errors, and plain objects recursively; platform objects
// that register a clone hook (see crypto.subtle, registered in the WebCrypto
// group) are routed there.
function _structuredClone(value, seen) {
  // Functions and symbols are not structured-cloneable (HTML structured clone,
  // DataCloneError). This must run before the primitive early-return below,
  // which would otherwise pass them through by reference.
  if (typeof value === "function" || typeof value === "symbol") {
    throw new DOMException("Failed to execute 'structuredClone': value could not be cloned.", "DataCloneError");
  }
  if (value === null || typeof value !== "object") return value;
  if (seen.has(value)) return seen.get(value);
  // Typed arrays: copy the underlying buffer slice. DataView has no .slice(),
  // so slice its buffer over the view's range and wrap a fresh view.
  if (ArrayBuffer.isView(value)) {
    if (value instanceof DataView) {
      const buf = value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength);
      const copy = new DataView(buf);
      seen.set(value, copy);
      return copy;
    }
    const Ctor = value.constructor;
    const copy = new Ctor(value.slice());
    seen.set(value, copy);
    return copy;
  }
  if (value instanceof ArrayBuffer) {
    const copy = value.slice(0);
    seen.set(value, copy);
    return copy;
  }
  if (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer) {
    return value; // transferable, not copyable
  }
  if (value instanceof Date) return new Date(value.getTime());
  if (value instanceof RegExp) return new RegExp(value.source, value.flags);
  if (value instanceof Map) {
    const m = new Map();
    seen.set(value, m);
    for (const [k, v] of value) m.set(_structuredClone(k, seen), _structuredClone(v, seen));
    return m;
  }
  if (value instanceof Set) {
    const s = new Set();
    seen.set(value, s);
    for (const v of value) s.add(_structuredClone(v, seen));
    return s;
  }
  if (value instanceof Error) {
    const Ctor = value.constructor || Error;
    const e = new Ctor(value.message);
    // Record the clone before recursing into `cause`, otherwise a cycle
    // through the error (e.cause === e) recurses until the stack overflows.
    seen.set(value, e);
    if (value.name) e.name = value.name;
    if (value.stack) e.stack = value.stack;
    if (value.cause !== undefined) e.cause = _structuredClone(value.cause, seen);
    return e;
  }
  // Platform objects that carry internal slots opt into cloning via a hook
  // (CryptoKey re-registers its key material so the clone stays usable by
  // crypto.subtle). Anything else with a registered hook takes that path.
  if (typeof value[Symbol.toStringTag] === "string" && globalThis.__diting_clone_hooks) {
    const hook = globalThis.__diting_clone_hooks[value[Symbol.toStringTag]];
    if (typeof hook === "function") return hook(value, seen);
  }
  // Plain objects clone onto Object.prototype (like Chrome), not the source's
  // prototype. Define each property instead of assigning it: a source with an
  // own enumerable `__proto__` data prop (what JSON.parse('{"__proto__":…}')
  // yields) would otherwise hit the inherited __proto__ setter and reparent
  // the clone instead of copying the property.
  const out = Array.isArray(value) ? [] : {};
  seen.set(value, out);
  for (const k in value) {
    if (Object.prototype.hasOwnProperty.call(value, k)) {
      const cloned = _structuredClone(value[k], seen);
      // Only `__proto__` needs defineProperty: plain assignment would hit the
      // inherited prototype setter and reparent the clone instead of adding an
      // own data property. Every other key takes the fast assignment path.
      if (k === "__proto__") {
        Object.defineProperty(out, k, {
          value: cloned,
          writable: true,
          enumerable: true,
          configurable: true,
        });
      } else {
        out[k] = cloned;
      }
    }
  }
  // Symbols are not enumerable via for-in; copy own symbol-keyed properties.
  const syms = Object.getOwnPropertySymbols(value);
  for (const s of syms) {
    const d = Object.getOwnPropertyDescriptor(value, s);
    if (d && "value" in d) out[s] = _structuredClone(d.value, seen);
  }
  return out;
}
globalThis.structuredClone = globalThis.structuredClone || ((v) => _structuredClone(v, new Map()));
globalThis.reportError = globalThis.reportError || ((e) => console.error(e));

// WHATWG Storage as a legacy platform object: a Proxy routes property access
// (localStorage.foo, localStorage["foo"], delete, `in`, Object.keys) through
// the named getter/setter so length/key()/iteration stay in sync with the
// backing map. Plain prototype methods alone could not intercept direct
// property access, so `localStorage.foo = x` never updated length before.
globalThis.Storage = function Storage() {};
Storage.prototype.getItem = function(k) { k = String(k); return Object.prototype.hasOwnProperty.call(this._data, k) ? this._data[k] : null; };
Storage.prototype.setItem = function(k, v) { this._data[String(k)] = String(v); };
Storage.prototype.removeItem = function(k) { delete this._data[String(k)]; };
Storage.prototype.clear = function() { const d = this._data; for (const k in d) delete d[k]; };
Storage.prototype.key = function(i) { const ks = Object.keys(this._data); i = i >>> 0; return i < ks.length ? ks[i] : null; };
Object.defineProperty(Storage.prototype, 'length', { get: function() { return Object.keys(this._data).length; }, configurable: true });

const _mkStore = () => {
  const target = Object.create(Storage.prototype);
  Object.defineProperty(target, '_data', { value: Object.create(null), writable: true, enumerable: false, configurable: true });
  const isReal = (p) => p === '_data' || p === 'constructor' || (p in Storage.prototype);
  return new Proxy(target, {
    get(t, p, recv) { if (typeof p === 'symbol' || isReal(p)) return Reflect.get(t, p, recv); const v = t.getItem(p); return v === null ? undefined : v; },
    set(t, p, v, recv) { if (typeof p === 'symbol' || isReal(p)) return Reflect.set(t, p, v, recv); t.setItem(p, v); return true; },
    has(t, p) { if (typeof p === 'symbol' || isReal(p)) return true; return Object.prototype.hasOwnProperty.call(t._data, p); },
    deleteProperty(t, p) { if (typeof p === 'symbol' || isReal(p)) return Reflect.deleteProperty(t, p); t.removeItem(p); return true; },
    ownKeys(t) { return Object.keys(t._data); },
    getOwnPropertyDescriptor(t, p) {
      if (typeof p !== 'symbol' && Object.prototype.hasOwnProperty.call(t._data, p))
        return { value: t._data[p], writable: true, enumerable: true, configurable: true };
      return Reflect.getOwnPropertyDescriptor(t, p);
    },
  });
};
globalThis.localStorage = _mkStore();
globalThis.sessionStorage = _mkStore();

globalThis.btoa = globalThis.btoa || ((s) => { const b = new TextEncoder().encode(s); const c="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let r=""; for(let i=0;i<b.length;i+=3){const a=b[i],bb=b[i+1]??0,cc=b[i+2]??0; r+=c[a>>2]+c[((a&3)<<4)|(bb>>4)]+(i+1<b.length?c[((bb&15)<<2)|(cc>>6)]:"=")+(i+2<b.length?c[cc&63]:"=");} return r; });
globalThis.atob = globalThis.atob || ((s) => { const c="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let r=[]; for(let i=0;i<s.length;i+=4){const a=c.indexOf(s[i]),b=c.indexOf(s[i+1]),cc=c.indexOf(s[i+2]),d=c.indexOf(s[i+3]); r.push((a<<2)|(b>>4)); if(cc>=0)r.push(((b&15)<<4)|(cc>>2)); if(d>=0)r.push(((cc&3)<<6)|d);} return String.fromCharCode(...r); });

// Functional History API. The earlier stub returned constant state and was a
// no-op on push/replace, so any SPA that tried to update its URL (Next.js
// client router, React Router, vue-router, hash-based routers) silently
// failed: location.href stayed pinned to the initial page, useLocation hooks
// never updated, and popstate-driven UI froze.
//
// Internally we keep a tiny in-memory stack of {state, url} entries. push/
// replace mutate the stack and set globalThis.__virtualUrl so location.href
// reads the new URL. Real Chrome doesn't fire popstate on push/replace,
// only on user-driven back/forward — we match that exactly.
(() => {
  const stack = [{state: null, url: undefined}]; // initial entry; url=undefined means "use document URL"
  let idx = 0;
  const resolveOrFallback = (url) => {
    // A missing url (pushState/replaceState called with < 3 args) keeps the
    // current document URL per the HTML spec — capture it so the entry does
    // not reset location back to the original document URL (upstream #496).
    if (url === null || url === undefined) return __currentUrl();
    try { return new URL(String(url), __currentUrl()).href; } catch (e) { return String(url); }
  };
  const applyVirtual = () => {
    const entry = stack[idx];
    globalThis.__virtualUrl = entry.url ?? null;
  };
  const fireHashChangeIfNeeded = (prevUrl) => {
    try {
      const next = __currentUrl();
      if (!prevUrl || !next) return;
      const a = new URL(prevUrl), b = new URL(next);
      if (a.origin === b.origin && a.pathname === b.pathname && a.search === b.search && a.hash !== b.hash) {
        const ev = new Event('hashchange');
        ev.oldURL = prevUrl; ev.newURL = next;
        try { globalThis.dispatchEvent(ev); } catch {}
      }
    } catch {}
  };
  globalThis.history = {
    get length() { return stack.length; },
    get state() { return stack[idx].state; },
    scrollRestoration: "auto",
    pushState(state, _title, url) {
      const prevUrl = __currentUrl();
      const resolved = resolveOrFallback(url);
      // Truncate forward entries (real Chrome drops the forward stack on a
      // new push) then append + advance.
      stack.length = idx + 1;
      stack.push({state: state ?? null, url: resolved});
      idx = stack.length - 1;
      applyVirtual();
      fireHashChangeIfNeeded(prevUrl);
    },
    replaceState(state, _title, url) {
      const prevUrl = __currentUrl();
      const resolved = resolveOrFallback(url);
      stack[idx] = {state: state ?? null, url: resolved};
      applyVirtual();
      fireHashChangeIfNeeded(prevUrl);
    },
    go(n) {
      n = (n | 0);
      if (n === 0) return; // real spec: go(0) reloads. We don't reload SPAs.
      const next = Math.max(0, Math.min(stack.length - 1, idx + n));
      if (next === idx) return;
      const prevUrl = __currentUrl();
      idx = next;
      applyVirtual();
      // Real Chrome fires popstate on back/forward with the destination entry's state.
      try {
        const ev = new PopStateEvent('popstate', {state: stack[idx].state});
        globalThis.dispatchEvent(ev);
      } catch {}
      fireHashChangeIfNeeded(prevUrl);
    },
    back() { this.go(-1); },
    forward() { this.go(1); },
  };
})();
globalThis.screenX = 0; globalThis.screenY = 0;
globalThis.screenLeft = 0; globalThis.screenTop = 0;
globalThis.pageXOffset = 0; globalThis.pageYOffset = 0;
globalThis.scrollX = 0; globalThis.scrollY = 0;

globalThis.CSS = { supports(){return false;}, escape(s){return s;} };

globalThis.HTMLElement = Element;
globalThis.HTMLDivElement = Element;
globalThis.HTMLSpanElement = Element;
globalThis.HTMLParagraphElement = Element;
globalThis.HTMLAnchorElement = Element;
globalThis.HTMLImageElement = Element;
globalThis.HTMLInputElement = Element;
globalThis.HTMLButtonElement = Element;
globalThis.HTMLFormElement = class HTMLFormElement extends Element {
  get elements() { return HTMLCollection._from(this.querySelectorAll("input, select, textarea, button, fieldset, output, object")); }
  get length() { return this.elements.length; }
  // Inherit submit() from Element.prototype: it dispatches the cancelable
  // 'submit' event and (if not prevented) builds form data and navigates.
  reset() { for (const f of this.elements) { if ('value' in f) f.value = ''; } }
};
globalThis.HTMLSelectElement = Element;
globalThis.HTMLTextAreaElement = Element;
globalThis.HTMLLabelElement = Element;
globalThis.HTMLTableElement = Element;
globalThis.HTMLIFrameElement = Element;
globalThis.HTMLCanvasElement = Element;
globalThis.HTMLVideoElement = Element;
globalThis.HTMLAudioElement = Element;
globalThis.HTMLScriptElement = Element;
globalThis.HTMLStyleElement = Element;
globalThis.HTMLLinkElement = Element;
globalThis.HTMLMetaElement = Element;
globalThis.HTMLHeadElement = Element;
globalThis.HTMLBodyElement = Element;
globalThis.HTMLHtmlElement = Element;
globalThis.HTMLBRElement = Element;
globalThis.HTMLHRElement = Element;
globalThis.HTMLUListElement = Element;
globalThis.HTMLOListElement = Element;
globalThis.HTMLLIElement = Element;
globalThis.HTMLPreElement = Element;
globalThis.HTMLHeadingElement = Element;
globalThis.HTMLTemplateElement = Element;
globalThis.HTMLSlotElement = Element;
globalThis.HTMLOptionElement = Element;
globalThis.HTMLDataListElement = Element;
globalThis.HTMLFieldSetElement = Element;
globalThis.HTMLLegendElement = Element;
globalThis.HTMLProgressElement = Element;
globalThis.HTMLDetailsElement = Element;
globalThis.HTMLDialogElement = Element;
globalThis.SVGElement = Element;
globalThis.SVGSVGElement = Element;
// API-tamper probes check these globals exist with the listed methods on
// their prototypes (e.g. `SVGTextContentElement.prototype.getExtentOfChar`).
globalThis.SVGGraphicsElement = class SVGGraphicsElement extends Element {
  getBBox() { return { x: 0, y: 0, width: 0, height: 0 }; }
  getScreenCTM() { return null; }
  getCTM() { return null; }
};
globalThis.SVGTextContentElement = class SVGTextContentElement extends Element {
  getExtentOfChar() { return { x: 0, y: 0, width: 0, height: 0 }; }
  getSubStringLength() { return 0; }
  getComputedTextLength() { return 0; }
};
globalThis.TextMetrics = class TextMetrics {
  constructor() { this.width = 0; this.actualBoundingBoxLeft = 0; this.actualBoundingBoxRight = 0;
    this.actualBoundingBoxAscent = 0; this.actualBoundingBoxDescent = 0; }
};
globalThis.GPUAdapter = class GPUAdapter {
  requestAdapterInfo() { return Promise.resolve({ vendor: 'google', architecture: 'unknown', device: '', description: '' }); }
  requestDevice() { return Promise.reject(new Error('GPUDevice unavailable')); }
};
globalThis.GPU = class GPU {
  requestAdapter() { return Promise.resolve(null); }
  getPreferredCanvasFormat() { return 'bgra8unorm'; }
};
globalThis.CharacterData = CharacterData;
globalThis.Text = Text;
globalThis.Comment = Comment;

globalThis.CDATASection = CDATASection;
globalThis.ProcessingInstruction = ProcessingInstruction;
// True when the document was loaded from an XML/XHTML source. Obscura has no
// native XML tree, so this is inferred from contentType (derived from the URL).
function _isXMLDocument(doc) {
  const ct = (doc && doc.contentType) || "text/html";
  return ct !== "text/html";
}
// XML Name production, sufficient for createProcessingInstruction targets.
const _piNameStart = "A-Za-z_:\\u00C0-\\u00D6\\u00D8-\\u00F6\\u00F8-\\u02FF\\u0370-\\u037D\\u037F-\\u1FFF\\u200C-\\u200D\\u2070-\\u218F\\u2C00-\\u2FEF\\u3001-\\uD7FF\\uF900-\\uFDCF\\uFDF0-\\uFFFD";
const _piNameChar = _piNameStart + "0-9.\\u00B7\\u0300-\\u036F\\u203F-\\u2040\\-";
const _piNameRe = new RegExp("^[" + _piNameStart + "][" + _piNameChar + "]*$");
function _isValidPITarget(target) {
  return typeof target === "string" && target.length > 0 && _piNameRe.test(target);
}
globalThis.DocumentFragment = DocumentFragment;
globalThis.DocumentType = DocumentType;
globalThis.Node = Node;
globalThis.Element = Element;
globalThis.Document = Document;
// Type of element.style / getComputedStyle(); the class binding above is
// lexical, so without this `window.CSSStyleDeclaration` was undefined and
// `el.style instanceof CSSStyleDeclaration` threw (upstream a0e1ba5).
globalThis.CSSStyleDeclaration = CSSStyleDeclaration;
globalThis.DOMStringMap = DOMStringMap;
// XMLDocument is a subclass of Document (DOMParser of an XML type and
// implementation.createDocument produce one). The interface must exist globally.
if (typeof XMLDocument === "undefined") globalThis.XMLDocument = class XMLDocument extends Document {};
// ParentNode mixin: Document and DocumentFragment are ParentNodes too, so they
// share Element's append / prepend / replaceChildren.
for (const _proto of [Document.prototype, DocumentFragment.prototype]) {
  _proto.append = Element.prototype.append;
  _proto.prepend = Element.prototype.prepend;
  _proto.replaceChildren = Element.prototype.replaceChildren;
}
globalThis.EventTarget = Node;
globalThis.HTMLCollection = class HTMLCollection extends Array {
  item(i) {
    i = i >>> 0;
    return this[i] != null ? this[i] : null;
  }
  namedItem(name) {
    if (name === undefined || name === null || name === "") return null;
    name = String(name);
    for (let i = 0; i < this.length; i++) {
      const el = this[i];
      if (!el) continue;
      // id always contributes; name only for HTML elements in HTML documents.
      if (el.id === name) return el;
      if (_isHTMLEl(el) && typeof el.getAttribute === "function" && el.getAttribute("name") === name) return el;
    }
    return null;
  }
  // Factory: build an HTMLCollection from an array of elements. Named access
  // (collection[name]) is served lazily by a Proxy so there is NO per-element
  // work at build time (eager defineProperty per id was an O(n) build cost that
  // made querySelectorAll on large result sets ~26x slower). The Proxy only
  // resolves a name when an unknown string key is actually read.
  static _from(arr) {
    const c = new HTMLCollection();
    if (arr) for (let i = 0; i < arr.length; i++) { if (arr[i]) c[c.length] = arr[i]; }
    return new Proxy(c, _htmlCollectionProxy);
  }
};
_markNative(HTMLCollection.prototype.item);
_markNative(HTMLCollection.prototype.namedItem);
// Shared (allocated once) Proxy traps for HTMLCollection named access. Indices,
// length, and inherited methods resolve normally via Reflect; only an unknown
// non-numeric string key falls back to namedItem(), so item/namedItem and the
// Array methods are never shadowed and id="namedItem" cannot recurse.
const _htmlCollectionProxy = {
  get(t, k, r) {
    const v = Reflect.get(t, k, r);
    if (v !== undefined || typeof k !== "string") return v;
    return t.namedItem ? (t.namedItem(k) || undefined) : undefined;
  },
  has(t, k) {
    if (Reflect.has(t, k)) return true;
    return typeof k === "string" && !!(t.namedItem && t.namedItem(k));
  },
};
// True for elements in the HTML namespace (the only ones whose name attribute
// contributes to an HTMLCollection's supported property names).
function _isHTMLEl(el) {
  return !!el && (el.namespaceURI === undefined || el.namespaceURI === "http://www.w3.org/1999/xhtml");
}
// Build a NodeList (no named access, per spec) for querySelectorAll and
// childNodes. Kept light on purpose: querySelectorAll is the hottest query API.
function _nodeList(els) {
  const nl = new NodeList();
  for (let i = 0; i < els.length; i++) nl[i] = els[i];
  nl.length = els.length;
  return nl;
}
globalThis.DOMTokenList = DOMTokenList;
// NodeList is its own type, not an Array subclass: in a real browser
// Array.isArray(nodeList) is false and Object.prototype.toString reports
// "[object NodeList]". Fingerprinting and feature-detection scripts check both.
// It keeps the array-like surface scripts actually use: indexed access, length,
// item(), forEach(), entries/keys/values, and iteration (so spread and for..of
// work).
globalThis.NodeList = class NodeList {
  constructor() { this.length = 0; }
  item(i) { i = i >>> 0; return this[i] != null ? this[i] : null; }
  forEach(cb, thisArg) {
    for (let i = 0; i < this.length; i++) cb.call(thisArg, this[i], i, this);
  }
  *[Symbol.iterator]() { for (let i = 0; i < this.length; i++) yield this[i]; }
  *entries() { for (let i = 0; i < this.length; i++) yield [i, this[i]]; }
  *keys() { for (let i = 0; i < this.length; i++) yield i; }
  *values() { for (let i = 0; i < this.length; i++) yield this[i]; }
  get [Symbol.toStringTag]() { return 'NodeList'; }
};
_markNative(NodeList);
_markNative(NodeList.prototype.item);
_markNative(NodeList.prototype.forEach);
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
  constructor(doc) { this._doc = doc; this._range = null; this._direction = 'none'; }
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

[
  navigator.getBattery, navigator.getGamepads, navigator.sendBeacon,
  navigator.javaEnabled, navigator.geolocation?.getCurrentPosition,
  navigator.geolocation?.watchPosition,
  navigator.serviceWorker?.register,
  navigator.permissions?.query, navigator.credentials?.get,
  navigator.storage?.estimate, navigator.storage?.persist, navigator.storage?.persisted,
  globalThis.fetch, globalThis.matchMedia, globalThis.getComputedStyle,
  globalThis.getSelection, globalThis.requestAnimationFrame,
  globalThis.cancelAnimationFrame, globalThis.setTimeout, globalThis.clearTimeout,
  globalThis.setInterval, globalThis.clearInterval, globalThis.queueMicrotask,
  globalThis.structuredClone, globalThis.reportError,
  globalThis.btoa, globalThis.atob,
  console.log, console.warn, console.error, console.info, console.debug,
  console.dir, console.assert,
  Element.prototype.getAttribute, Element.prototype.setAttribute,
  Element.prototype.removeAttribute, Element.prototype.hasAttribute,
  Element.prototype.querySelector, Element.prototype.querySelectorAll,
  Element.prototype.getElementsByTagName, Element.prototype.getElementsByClassName,
  Element.prototype.matches, Element.prototype.closest,
  Element.prototype.getBoundingClientRect, Element.prototype.getClientRects,
  Element.prototype.checkVisibility,
  Element.prototype.addEventListener, Element.prototype.removeEventListener,
  Element.prototype.dispatchEvent, Element.prototype.click,
  Element.prototype.focus, Element.prototype.blur,
  Element.prototype.showPopover, Element.prototype.hidePopover, Element.prototype.togglePopover,
  Element.prototype.cloneNode, Element.prototype.attachShadow,
  Element.prototype.insertAdjacentHTML, Element.prototype.scrollIntoView,
  Element.prototype.scrollTo, Element.prototype.scrollBy, Element.prototype.scroll,
  Element.prototype.append, Element.prototype.prepend, Element.prototype.remove,
  Element.prototype.before, Element.prototype.after, Element.prototype.replaceWith,
  HTMLFormElement.prototype.reset,
  Element.prototype.getContext, Element.prototype.toDataURL, Element.prototype.toBlob,
  Node.prototype.appendChild, Node.prototype.removeChild,
  Node.prototype.replaceChild, Node.prototype.insertBefore,
  Node.prototype.contains, Node.prototype.hasChildNodes, Node.prototype.cloneNode,
  CharacterData.prototype.before, CharacterData.prototype.after,
  CharacterData.prototype.replaceWith, CharacterData.prototype.remove,
  Document.prototype.getElementById, Document.prototype.querySelector,
  Document.prototype.querySelectorAll, Document.prototype.getElementsByTagName,
  Document.prototype.createElement, Document.prototype.createElementNS,
  Document.prototype.createTextNode, Document.prototype.createComment,
  Document.prototype.createCDATASection, Document.prototype.createProcessingInstruction,
  Document.prototype.createDocumentFragment, Document.prototype.createEvent,
  Document.prototype.hasFocus,
  Storage, Storage.prototype.getItem, Storage.prototype.setItem,
  Storage.prototype.removeItem, Storage.prototype.clear, Storage.prototype.key,
  Notification, Notification.requestPermission,
  window.chrome?.csi, window.chrome?.loadTimes,
  MutationObserver, ResizeObserver, IntersectionObserver, PerformanceObserver,
  XMLSerializer, XMLSerializer.prototype.serializeToString,
].forEach(fn => { if (typeof fn === 'function') _markNative(fn); });

class _IframeDocument {
  constructor(html, url, iframeEl) {
    this._url = url;
    this._iframeEl = iframeEl;
    this.nodeType = 9;
    this.nodeName = '#document';
    this.readyState = 'complete';
    this.characterSet = 'UTF-8';
    this.contentType = 'text/html';
    this.visibilityState = 'visible';
    this.hidden = false;

    this._root = document.createElement('html');
    this._head = document.createElement('head');
    this._body = document.createElement('body');
    this._root.appendChild(this._head);
    this._root.appendChild(this._body);
    var bodyContent = html
      .replace(/^<!DOCTYPE[^>]*>/i, '')
      .replace(/<\/?html[^>]*>/gi, '')
      .replace(/<head[^>]*>[\s\S]*?<\/head>/gi, '')
      .replace(/<\/?body[^>]*>/gi, '')
      .replace(/^\s+/, ''); // trim leading whitespace (before <body> content)
    if (bodyContent) {
      this._body.innerHTML = bodyContent;
    }

    this._title = '';
    if (this._head) {
      const titleEl = this._head.querySelector('title');
      if (titleEl) this._title = titleEl.textContent;
    }
  }

  get documentElement() { return this._root; }
  get head() { return this._head; }
  get body() { return this._body; }
  get title() { return this._title; }
  set title(v) { this._title = v; }
  get URL() { return this._url; }
  get documentURI() { return this._url; }
  get location() { return this._iframeEl?.contentWindow?.location; }
  get defaultView() { return this._iframeEl?.contentWindow; }
  get ownerDocument() { return null; }
  get compatMode() { return 'CSS1Compat'; }
  get activeElement() { return this._body; }

  getElementById(id) {
    return this._root.querySelector('#' + id);
  }
  querySelector(sel) {
    return this._root.querySelector(sel);
  }
  querySelectorAll(sel) {
    return this._root.querySelectorAll(sel);
  }
  getElementsByTagName(tag) {
    return this._root.querySelectorAll(tag);
  }
  getElementsByClassName(cls) {
    return _getElementsByClassName(this._root, cls);
  }
  createElement(tag) { return document.createElement(tag); }
  createElementNS(ns, tag) { return document.createElementNS(ns, tag); }
  createTextNode(text) { return document.createTextNode(text); }
  createComment(text) { return document.createComment(text); }
  createDocumentFragment() { return document.createDocumentFragment(); }
  createEvent(type) { return document.createEvent(type); }
  createRange() { return new Range(); }
  hasFocus() { return false; }

  get cookie() { return ''; }
  set cookie(v) {}
  get implementation() { return document.implementation; }
  get styleSheets() { return []; }

  // Listeners registered on an iframe document used to never run (these were
  // no-ops), so `iframeDoc.addEventListener('DOMContentLoaded', ...)` and the
  // like silently did nothing (upstream #478).
  addEventListener(type, listener) {
    if (typeof listener !== 'function') return;
    if (!this._listeners) this._listeners = Object.create(null);
    const list = this._listeners[type] || (this._listeners[type] = []);
    if (!list.includes(listener)) list.push(listener);
  }
  removeEventListener(type, listener) {
    const list = this._listeners && this._listeners[type];
    if (!list) return;
    const index = list.indexOf(listener);
    if (index !== -1) list.splice(index, 1);
  }
  dispatchEvent(event) {
    const type = event && event.type;
    if (!type) return true;
    const list = this._listeners && this._listeners[type];
    if (list) {
      for (const listener of list.slice()) {
        try { listener.call(this, event); } catch (error) { console.error(error); }
      }
    }
    const handler = this['on' + type];
    if (typeof handler === 'function') {
      try { handler.call(this, event); } catch (error) { console.error(error); }
    }
    return !event.defaultPrevented;
  }

  write(html) {
    if (this._body) this._body.innerHTML += html;
  }
  writeln(html) { this.write(html + '\n'); }
  open() { if (this._body) this._body.innerHTML = ''; }
  close() {}
}

class _IframeWindow {
  constructor(doc, url) {
    this.document = doc;
    this._url = url;
    this.self = this;
    this.top = globalThis;
    this.parent = globalThis;
    this.window = this;
    this.frames = this;
    this.frameElement = null;
    this.length = 0;
    this.name = '';
    this.closed = false;
    this.navigator = globalThis.navigator;
    this.screen = globalThis.screen;
    this.innerWidth = 300;
    this.innerHeight = 150;
    this.outerWidth = 300;
    this.outerHeight = 150;
    this.devicePixelRatio = globalThis.devicePixelRatio;
    this.localStorage = globalThis.localStorage;
    this.sessionStorage = globalThis.sessionStorage;
    this.performance = globalThis.performance;
    this.crypto = globalThis.crypto;
    this.console = globalThis.console;
    this.chrome = globalThis.chrome;

    try {
      const u = new URL(url);
      this.location = {
        href: url, origin: u.origin, protocol: u.protocol,
        host: u.host, hostname: u.hostname, port: u.port,
        pathname: u.pathname, search: u.search, hash: u.hash,
        toString() { return url; }, assign(){}, reload(){}, replace(){},
      };
    } catch(e) {
      this.location = { href: url, origin: '', protocol: '', host: '', hostname: '', port: '', pathname: '/', search: '', hash: '', toString() { return url; }, assign(){}, reload(){}, replace(){} };
    }
  }

  postMessage(data, targetOrigin) {
    // HTML spec: delivery happens only when targetOrigin is undefined/'*',
    // '/' (same-origin as the calling document), or matches THIS target
    // window's origin. Upstream obscura discarded the argument entirely
    // (#704): a caller restricting delivery to a trusted origin still had
    // the message delivered to whatever frame happened to be there — a
    // cross-origin data leak. Mismatches drop silently, like browsers.
    let t = targetOrigin;
    if (t === undefined || t === null) t = '*';
    if (typeof t !== 'string' || t === '') return;
    if (t !== '*') {
      const selfOrigin = this.location.origin || '';
      if (t === '/') {
        const pageOrigin = (function() { try { return new URL(_domParse("document_url") || "about:blank").origin; } catch(e) { return ''; } })();
        if (pageOrigin === '' || pageOrigin !== selfOrigin) return;
      } else {
        let tOrigin = '';
        try { tOrigin = new URL(t).origin; } catch(e) {}
        if (tOrigin === '' || tOrigin !== selfOrigin) return;
      }
    }
    const event = new MessageEvent('message', {
      data: data,
      origin: this.location.origin,
      source: this,
    });
    Promise.resolve().then(() => {
      globalThis.dispatchEvent?.(event);
    });
  }

  setTimeout(fn, ms) { return globalThis.setTimeout(fn, ms); }
  clearTimeout(id) { globalThis.clearTimeout(id); }
  setInterval(fn, ms) { return globalThis.setInterval(fn, ms); }
  clearInterval(id) { globalThis.clearInterval(id); }
  requestAnimationFrame(fn) { return globalThis.requestAnimationFrame(fn); }

  addEventListener(type, fn) {
    if (!this._listeners) this._listeners = {};
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(fn);
  }
  removeEventListener(type, fn) {
    if (this._listeners?.[type]) {
      this._listeners[type] = this._listeners[type].filter(h => h !== fn);
    }
  }
  dispatchEvent(event) {
    const handlers = this._listeners?.[event?.type] || [];
    for (const h of handlers) { try { h.call(this, event); } catch(e) {} }
    return true;
  }

  getComputedStyle(el) { return globalThis.getComputedStyle(el); }
  matchMedia(q) { return globalThis.matchMedia(q); }
  getSelection() { return globalThis.getSelection(); }
  fetch(input, init) { return globalThis.fetch(input, init); }
  close() { this.closed = true; }
  focus() {}
  blur() {}
}

globalThis.__ariaQuerySelector = function(root, selector) { return null; };
globalThis.__ariaQuerySelectorAll = async function*(root, selector) { /* yields nothing */ };
class _Canvas2D {
  constructor(canvas) {
    this.canvas = canvas;
    this._w = canvas.width || 300;
    this._h = canvas.height || 150;
    this._buf = new Uint8ClampedArray(this._w * this._h * 4);
    for (let i = 0; i < this._w * this._h; i++) {
      this._buf[i*4+0] = 255 + Math.floor(_fpNoise(i % this._w, Math.floor(i / this._w), 0));
      this._buf[i*4+1] = 255 + Math.floor(_fpNoise(i % this._w, Math.floor(i / this._w), 1));
      this._buf[i*4+2] = 255 + Math.floor(_fpNoise(i % this._w, Math.floor(i / this._w), 2));
      this._buf[i*4+3] = 255;
    }
    this.fillStyle = '#000000';
    this.strokeStyle = '#000000';
    this.lineWidth = 1;
    this.font = '10px sans-serif';
    this.textAlign = 'start';
    this.textBaseline = 'alphabetic';
    this.globalAlpha = 1;
    this.globalCompositeOperation = 'source-over';
    this._stateStack = [];
  }
  _parseColor(css) {
    if (typeof css !== 'string') css = String(css ?? '');
    if (!css || css === 'none') return [0,0,0,0];
    if (css.startsWith('#')) {
      const hex = css.slice(1);
      if (hex.length === 3) return [parseInt(hex[0]+hex[0],16),parseInt(hex[1]+hex[1],16),parseInt(hex[2]+hex[2],16),255];
      if (hex.length === 6) return [parseInt(hex.slice(0,2),16),parseInt(hex.slice(2,4),16),parseInt(hex.slice(4,6),16),255];
      if (hex.length === 8) return [parseInt(hex.slice(0,2),16),parseInt(hex.slice(2,4),16),parseInt(hex.slice(4,6),16),parseInt(hex.slice(6,8),16)];
    }
    const m = css.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)/);
    if (m) return [+m[1],+m[2],+m[3],m[4]!==undefined?Math.round(+m[4]*255):255];
    const named = {red:[255,0,0,255],green:[0,128,0,255],blue:[0,0,255,255],white:[255,255,255,255],black:[0,0,0,255],yellow:[255,255,0,255],orange:[255,165,0,255],gray:[128,128,128,255],transparent:[0,0,0,0]};
    return named[css] || [0,0,0,255];
  }
  _setPixel(x, y, r, g, b, a) {
    x = Math.round(x); y = Math.round(y);
    if (x < 0 || x >= this._w || y < 0 || y >= this._h) return;
    const idx = (y * this._w + x) * 4;
    const alpha = (a / 255) * this.globalAlpha;
    this._buf[idx+0] = Math.round(r * alpha + this._buf[idx+0] * (1 - alpha));
    this._buf[idx+1] = Math.round(g * alpha + this._buf[idx+1] * (1 - alpha));
    this._buf[idx+2] = Math.round(b * alpha + this._buf[idx+2] * (1 - alpha));
    this._buf[idx+3] = Math.min(255, Math.round(a * alpha + this._buf[idx+3] * (1 - alpha)));
  }
  fillRect(x, y, w, h) {
    const [r,g,b,a] = this._parseColor(this.fillStyle);
    x=Math.round(x); y=Math.round(y); w=Math.round(w); h=Math.round(h);
    for (let py = Math.max(0,y); py < Math.min(this._h, y+h); py++) {
      for (let px = Math.max(0,x); px < Math.min(this._w, x+w); px++) {
        this._setPixel(px, py, r, g, b, a);
      }
    }
  }
  clearRect(x, y, w, h) {
    x=Math.round(x); y=Math.round(y); w=Math.round(w); h=Math.round(h);
    for (let py = Math.max(0,y); py < Math.min(this._h, y+h); py++) {
      for (let px = Math.max(0,x); px < Math.min(this._w, x+w); px++) {
        const idx = (py * this._w + px) * 4;
        this._buf[idx] = this._buf[idx+1] = this._buf[idx+2] = this._buf[idx+3] = 0;
      }
    }
  }
  strokeRect(x, y, w, h) {
    const [r,g,b,a] = this._parseColor(this.strokeStyle);
    const lw = this.lineWidth;
    for (let px = Math.round(x); px < Math.round(x+w); px++) {
      for (let l = 0; l < lw; l++) { this._setPixel(px, Math.round(y)+l, r,g,b,a); this._setPixel(px, Math.round(y+h)-1-l, r,g,b,a); }
    }
    for (let py = Math.round(y); py < Math.round(y+h); py++) {
      for (let l = 0; l < lw; l++) { this._setPixel(Math.round(x)+l, py, r,g,b,a); this._setPixel(Math.round(x+w)-1-l, py, r,g,b,a); }
    }
  }
  fillText(text, x, y) {
    const [r,g,b,a] = this._parseColor(this.fillStyle);
    const fontSize = parseInt(this.font) || 10;
    const scale = Math.max(1, Math.round(fontSize / 10));
    const str = String(text);
    let cx = Math.round(x);
    for (let i = 0; i < str.length; i++) {
      const code = str.charCodeAt(i);
      for (let row = 0; row < 7; row++) {
        for (let col = 0; col < 5; col++) {
          const on = ((_fpRand(code * 100 + row * 10 + col) > 0.45) &&
                      (row > 0 && row < 6 && col > 0 && col < 4)) ||
                     (_fpRand(code * 200 + row * 7 + col) > 0.7);
          if (on) {
            for (let sy = 0; sy < scale; sy++) {
              for (let sx = 0; sx < scale; sx++) {
                this._setPixel(cx + col*scale + sx, Math.round(y) - 7*scale + row*scale + sy, r, g, b, a);
              }
            }
          }
        }
      }
      cx += 6 * scale;
    }
  }
  strokeText(text, x, y) { this.fillText(text, x, y); }
  measureText(t) {
    const fontSize = parseInt(this.font) || 10;
    const scale = Math.max(1, Math.round(fontSize / 10));
    // Per-font width factor so font-presence probes (measure the same
    // string across many family names; available fonts change the width,
    // missing ones fall back to the default) see a realistic spread.
    // Shares the persona's installed-font table with _obscuraFontBox so
    // canvas probing and offsetWidth probing agree on which faces exist.
    const fam = String(this.font || '');
    let factor = 1;
    for (const name of fam.split(',').map((s) => s.trim().replace(/^['"]|['"]$/g, ''))) {
      const f = _installedFontFactor(name);
      if (f !== null) { factor = f; break; }
    }
    const w = String(t).length * 6 * scale * factor;
    return { width: Math.round(w * 100) / 100, actualBoundingBoxAscent: 7*scale, actualBoundingBoxDescent: 2*scale };
  }
  getImageData(x, y, w, h) {
    x=Math.round(x); y=Math.round(y); w=Math.round(w); h=Math.round(h);
    const data = new Uint8ClampedArray(w * h * 4);
    for (let py = 0; py < h; py++) {
      for (let px = 0; px < w; px++) {
        const srcX = x + px, srcY = y + py;
        const dstIdx = (py * w + px) * 4;
        if (srcX >= 0 && srcX < this._w && srcY >= 0 && srcY < this._h) {
          const srcIdx = (srcY * this._w + srcX) * 4;
          data[dstIdx] = this._buf[srcIdx];
          data[dstIdx+1] = this._buf[srcIdx+1];
          data[dstIdx+2] = this._buf[srcIdx+2];
          data[dstIdx+3] = this._buf[srcIdx+3];
        }
      }
    }
    return { data, width: w, height: h };
  }
  putImageData(imageData, dx, dy) {
    dx=Math.round(dx); dy=Math.round(dy);
    const {data, width: w, height: h} = imageData;
    for (let py = 0; py < h; py++) {
      for (let px = 0; px < w; px++) {
        const srcIdx = (py * w + px) * 4;
        const x = dx + px, y = dy + py;
        if (x >= 0 && x < this._w && y >= 0 && y < this._h) {
          const dstIdx = (y * this._w + x) * 4;
          this._buf[dstIdx] = data[srcIdx];
          this._buf[dstIdx+1] = data[srcIdx+1];
          this._buf[dstIdx+2] = data[srcIdx+2];
          this._buf[dstIdx+3] = data[srcIdx+3];
        }
      }
    }
  }
  createImageData(w, h) { return { data: new Uint8ClampedArray(w*h*4), width: w, height: h }; }
  drawImage(img, sx, sy, sw, sh, dx, dy, dw, dh) {
    if (img && img._ctx && img._ctx._buf) {
      const src = img._ctx;
      dx = dx ?? sx; dy = dy ?? sy; dw = dw ?? (sw ?? src._w); dh = dh ?? (sh ?? src._h);
      for (let py = 0; py < dh; py++) {
        for (let px = 0; px < dw; px++) {
          const srcX = Math.floor((sx||0) + px * (sw||src._w) / dw);
          const srcY = Math.floor((sy||0) + py * (sh||src._h) / dh);
          if (srcX >= 0 && srcX < src._w && srcY >= 0 && srcY < src._h) {
            const srcIdx = (srcY * src._w + srcX) * 4;
            this._setPixel(dx+px, dy+py, src._buf[srcIdx], src._buf[srcIdx+1], src._buf[srcIdx+2], src._buf[srcIdx+3]);
          }
        }
      }
    }
  }
  beginPath() { this._path = []; }
  closePath() {}
  moveTo(x, y) { if (this._path) this._path.push({t:'M',x,y}); }
  lineTo(x, y) { if (this._path) this._path.push({t:'L',x,y}); }
  bezierCurveTo() {} quadraticCurveTo() {}
  arc(x, y, r, s, e) { if (this._path) this._path.push({t:'A',x,y,r}); }
  arcTo() {}
  rect(x, y, w, h) { this.fillRect(x, y, w, h); }
  fill() {}
  stroke() {}
  clip() {}
  save() { this._stateStack.push({fillStyle: this.fillStyle, strokeStyle: this.strokeStyle, globalAlpha: this.globalAlpha, font: this.font, lineWidth: this.lineWidth}); }
  restore() { const s = this._stateStack.pop(); if (s) Object.assign(this, s); }
  translate() {} rotate() {} scale() {}
  setTransform() {} resetTransform() {} transform() {}
  createLinearGradient(x0,y0,x1,y1) { return { addColorStop(){}, _x0:x0,_y0:y0,_x1:x1,_y1:y1 }; }
  createRadialGradient() { return { addColorStop(){} }; }
  createPattern() { return {}; }
  isPointInPath() { return false; }
  isPointInStroke() { return false; }
}

Element.prototype.getContext = function getContext(type) {
  if (type === '2d') {
    if (!this._ctx) {
      this._ctx = new _Canvas2D(this);
    }
    return this._ctx;
  }
  if (type === 'webgl' || type === 'experimental-webgl' || type === 'webgl2') {
    if (this._glCtx) return this._glCtx;
    const base = {
      canvas: this,
      // Introspection methods (getParameter/getExtension/getSupportedExtensions/
      // getShaderPrecisionFormat) and all GL constants come from the
      // WebGL{,2}RenderingContext.prototype set up at bootstrap — own copies
      // here would shadow them.
      createBuffer() { return {}; }, createShader() { return {}; }, createProgram() { return {}; },
      shaderSource() {}, compileShader() {}, attachShader() {}, linkProgram() {},
      getProgramParameter() { return true; }, useProgram() {}, deleteShader() {},
      bindBuffer() {}, bufferData() {}, enableVertexAttribArray() {}, vertexAttribPointer() {},
      drawArrays() {}, drawElements() {}, viewport() {}, clear() {}, clearColor() {},
      enable() {}, disable() {}, blendFunc() {}, depthFunc() {},
      getUniformLocation() { return {}; }, getAttribLocation() { return 0; },
      uniform1f() {}, uniform1i() {}, uniform1fv() {}, uniform1iv() {},
      uniform2f() {}, uniform2i() {}, uniform2fv() {}, uniform2iv() {},
      uniform3f() {}, uniform3i() {}, uniform3fv() {}, uniform3iv() {},
      uniform4f() {}, uniform4i() {}, uniform4fv() {}, uniform4iv() {},
      uniformMatrix2fv() {}, uniformMatrix3fv() {}, uniformMatrix4fv() {},
      createTexture() { return {}; }, bindTexture() {}, texImage2D() {}, texParameteri() {},
      activeTexture() {}, pixelStorei() {}, generateMipmap() {},
      deleteBuffer() {}, deleteTexture() {}, deleteProgram() {}, deleteFramebuffer() {},
      getActiveUniform() { return null; }, getActiveAttrib() { return null; }, getUniform() { return null; },
      getProgramInfoLog() { return ''; }, getShaderInfoLog() { return ''; },
      getFramebufferStatus() { return 0x8CD5; }, checkFramebufferStatus() { return 0x8CD5; },
      getError() { return 0; }, isContextLost() { return false; },
      getContextAttributes() { return { alpha: true, antialias: true, depth: true, stencil: false, failIfMajorPerformanceCaveat: false, powerPreference: 'default', preserveDrawingBuffer: false, desynchronized: false, premultipliedAlpha: true }; },
      createFramebuffer() { return {}; }, bindFramebuffer() {}, framebufferTexture2D() {},
      VERTEX_SHADER: 0x8B31, FRAGMENT_SHADER: 0x8B30, LINK_STATUS: 0x8B82,
      ARRAY_BUFFER: 0x8892, STATIC_DRAW: 0x88E4, FLOAT: 0x1406,
      TRIANGLES: 0x0004, COLOR_BUFFER_BIT: 0x4000, DEPTH_BUFFER_BIT: 0x100,
      TEXTURE_2D: 0x0DE1, RGBA: 0x1908, UNSIGNED_BYTE: 0x1401,
    };
    // Prototype chain: `gl instanceof WebGLRenderingContext` must be true —
    // three.js and bot detectors check it.
    Object.setPrototypeOf(base, (type === 'webgl2')
      ? globalThis.WebGL2RenderingContext.prototype
      : globalThis.WebGLRenderingContext.prototype);
    // Proxy fallback: any WebGL method a library calls that we didn't
    // enumerate becomes a number-ish no-op instead of throwing TypeError.
    const numNoop = function() { return 0; };
    numNoop.valueOf = () => 0;
    numNoop.toString = () => "0";
    const gl = new Proxy(base, {
      get(target, prop) {
        // Symbols (Symbol.iterator, Symbol.toPrimitive, ...) must fall through
        // to the real value, and 'then' must stay undefined — returning
        // numNoop for it would make the context thenable and break
        // await/Promise.resolve on it.
        if (typeof prop === 'symbol' || prop === 'then' || prop === 'toJSON') return target[prop];
        if (prop in target) return target[prop];
        return numNoop;
      },
    });
    this._glCtx = gl;
    return gl;
  }
  return null;
};
Element.prototype.toDataURL = function(type) {
  if (this._ctx && this._ctx._buf) {
    const ctx = this._ctx;
    const w = ctx._w, h = ctx._h, buf = ctx._buf;
    let hash = _fpSeed;
    for (let i = 0; i < buf.length; i += 37) {
      hash = ((hash << 5) - hash + buf[i]) | 0;
    }
    const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    let b64 = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUg';
    for (let i = 0; i < 60; i++) {
      hash = ((hash << 5) - hash + i) | 0;
      b64 += chars[(hash >>> 0) % 64];
    }
    return b64 + '==';
  }
  return _fp('canvasFingerprint');
};
Element.prototype.toBlob = function(cb, type, q) { cb(new Blob([''])); };
// Chrome desktop media support matrix — WorkOS Radar's mediaMime collector
// probes audio/video elements with canPlayType over a fixed codec list and
// hashes the non-empty answers (real Chrome: 8 of 9).
Element.prototype.canPlayType = function(type) {
  const t = String(type || '').toLowerCase();
  if (!t) return '';
  if (/x-matroska/.test(t)) return '';
  if (/audio\/x-m4a|audio\/mp4|audio\/aiff/.test(t)) return 'maybe';
  if (/^audio\/(aac|mpeg|mp3|wav|ogg|flac)|^video\/(mp4|webm|ogg)/.test(t)) return 'probably';
  if (/codecs=("|')?(vp8|vp9|avc1|vorbis|opus|theora|1)("|')?/.test(t)) return 'probably';
  return '';
};
_markNative(Element.prototype.canPlayType);

_markNative(Element.prototype.getContext);
_markNative(Element.prototype.toDataURL);
_markNative(Element.prototype.toBlob);

Element.prototype.attachShadow = function attachShadow(opts) {
  var _mode = opts == null ? undefined : opts.mode;
  if (_mode !== 'open' && _mode !== 'closed') {
    throw new TypeError('Failed to execute attachShadow on Element: the mode value is not a valid ShadowRootMode.');
  }
  var _ln = (this.localName || '').toLowerCase();
  if (!globalThis.__diting_shadowHostNames.has(_ln) && _ln.indexOf('-') === -1) {
    throw new DOMException('Failed to execute attachShadow on Element: this element does not support attachShadow', 'NotSupportedError');
  }
  if (this._shadowRoot) {
    throw new DOMException('Failed to execute attachShadow on Element: the element already hosts a shadow tree.', 'NotSupportedError');
  }
  const host = this;
  const children = [];
  const shadow = {
    mode: opts.mode,
    host: host,
    get innerHTML() { return children.map(c => c.outerHTML || c.textContent || '').join(''); },
    set innerHTML(v) {
      children.length = 0;
      if (v) {
        const tmp = document.createElement('div');
        tmp.innerHTML = v;
        for (let i = 0; i < tmp.childNodes.length; i++) children.push(tmp.childNodes[i]);
      }
    },
    get childNodes() { return children; },
    get firstChild() { return children[0] || null; },
    get lastChild() { return children[children.length - 1] || null; },
    get firstElementChild() { return children.find(c => c.nodeType === 1) || null; },
    get children() { return children.filter(c => c.nodeType === 1); },
    appendChild(c) {
      if (c) {
        children.push(c);
        try { c.parentNode = shadow; } catch (_) { /* parentNode is getter-only on Node, ignore */ }
      }
      return c;
    },
    insertBefore(n, ref) {
      if (!n) return n;
      if (!ref) { shadow.appendChild(n); return n; }
      const idx = children.indexOf(ref);
      if (idx >= 0) {
        children.splice(idx, 0, n);
        try { n.parentNode = shadow; } catch (_) {}
      }
      else shadow.appendChild(n);
      return n;
    },
    removeChild(c) { const idx = children.indexOf(c); if (idx >= 0) children.splice(idx, 1); return c; },
    replaceChild(n, o) {
      const idx = children.indexOf(o);
      if (idx >= 0) {
        children[idx] = n;
        try { n.parentNode = shadow; } catch (_) {}
      }
      return o;
    },
    querySelector(s) {
      for (const c of children) {
        if (c.matches && c.matches(s)) return c;
        if (c.querySelector) { const r = c.querySelector(s); if (r) return r; }
      }
      return null;
    },
    querySelectorAll(s) {
      const results = [];
      for (const c of children) {
        if (c.matches && c.matches(s)) results.push(c);
        if (c.querySelectorAll) results.push(...c.querySelectorAll(s));
      }
      return results;
    },
    getElementById(id) { return shadow.querySelector('#' + id); },
    contains(n) { return children.includes(n); },
    getRootNode() { return shadow; },
    get ownerDocument() { return document; },
    get nodeType() { return 11; }, // DOCUMENT_FRAGMENT_NODE
    get nodeName() { return '#document-fragment'; },
    addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
    setHTMLUnsafe(v) { this.innerHTML = String(v == null ? "" : v); },
    getHTML() { return this.innerHTML; },
    // Own textContent: ShadowRoot now extends DocumentFragment, so without
    // these the inherited Node accessors run against this._nid. The setter in
    // particular would target the host document and wipe it. Operate on the
    // shadow's own `children` store instead.
    get textContent() { return children.map(c => c.textContent || "").join(""); },
    set textContent(v) {
      children.length = 0;
      if (v != null && v !== "") children.push(document.createTextNode(String(v)));
    },
    hasChildNodes() { return children.length > 0; },
    // A detached fragment id backs any inherited nid-based method we do not
    // override, so they stay non-destructive (operate on an empty fragment)
    // rather than falling through to node 0 / the document.
    _nid: +_dom("create_document_fragment"),
    activeElement: null,
    get styleSheets() { return []; },
    cloneNode() { throw new DOMException('Failed to execute cloneNode on Node: ShadowRoot nodes are not clonable.', 'NotSupportedError'); },
  };
  Object.setPrototypeOf(shadow, ShadowRoot.prototype);
  this._shadowRoot = shadow;
  return shadow;
};

_markNative(Element.prototype.attachShadow);

Object.defineProperty(Element.prototype, 'shadowRoot', {
  configurable: true,
  enumerable: true,
  get: function () {
    var sr = this._shadowRoot;
    return sr && sr.mode === 'open' ? sr : null;
  },
});

// setHTMLUnsafe / getHTML: shims over innerHTML. setHTMLUnsafe parses markup
// like innerHTML (declarative shadow roots inside are not expanded yet, but the
// call no longer throws so the rest of a test file can run); getHTML serializes
// like innerHTML.
Element.prototype.setHTMLUnsafe = function setHTMLUnsafe(html) { this.innerHTML = String(html == null ? "" : html); };
Element.prototype.getHTML = function getHTML() { return this.innerHTML; };
_markNative(Element.prototype.setHTMLUnsafe);
_markNative(Element.prototype.getHTML);
// Document.parseHTMLUnsafe(html): static that parses into a new HTML document.
if (typeof Document !== 'undefined' && typeof Document.parseHTMLUnsafe !== 'function') {
  Document.parseHTMLUnsafe = function parseHTMLUnsafe(html) {
    return new DOMParser().parseFromString(String(html == null ? "" : html), "text/html");
  };
  _markNative(Document.parseHTMLUnsafe);
}

function _audioParam(value, min, max) {
  const FLT_MAX = 3.4028234663852886e38;
  return {
    value, defaultValue: value,
    minValue: min === undefined ? -FLT_MAX : min,
    maxValue: max === undefined ? FLT_MAX : max,
    setValueAtTime(v) { this.value = v; return this; },
    linearRampToValueAtTime() { return this; },
    exponentialRampToValueAtTime() { return this; },
    setTargetAtTime() { return this; },
    setValueCurveAtTime() { return this; },
    cancelScheduledValues() { return this; },
    cancelAndHoldAtTime() { return this; },
  };
}
const _audioNodeBase = { connect(){}, disconnect(){}, addEventListener(){}, removeEventListener(){},
  channelCount: 2, channelCountMode: 'max', channelInterpretation: 'speakers', numberOfInputs: 1, numberOfOutputs: 1 };
// Every AudioNode exposes `.context` back to its AudioContext — fingerprint
// probes read `analyser.context.sampleRate`.
function _audioNode(ctx, extra) { return Object.assign({ context: ctx }, _audioNodeBase, extra); }
// Real AudioBuffer: detectors check `"copyFromChannel" in AudioBuffer.prototype`
// (ReferenceError if the global is missing) and verify copyFromChannel copies
// what getChannelData exposes — one backing Float32Array per channel.
globalThis.AudioBuffer = class AudioBuffer {
  constructor(options) {
    const o = options || {};
    const nc = Math.max(1, o.numberOfChannels || 1);
    const len = Math.max(0, (o.length | 0) || 0);
    this._channels = [];
    for (let i = 0; i < nc; i++) {
      this._channels.push(o.channelData && o.channelData[i]
        ? Float32Array.from(o.channelData[i])
        : new Float32Array(len));
    }
    this._length = len;
    this._sr = o.sampleRate || 44100;
  }
  get numberOfChannels() { return this._channels.length; }
  get length() { return this._length; }
  get sampleRate() { return this._sr; }
  get duration() { return this._length / this._sr; }
  getChannelData(c) { return this._channels[c] || (this._channels[c] = new Float32Array(this._length)); }
  copyFromChannel(dest, channel, bufferOffset) {
    const src = this.getChannelData(channel);
    const off = bufferOffset || 0;
    const n = Math.min(dest.length, src.length - off);
    for (let i = 0; i < n; i++) dest[i] = src[off + i];
  }
  copyToChannel(src, channel, bufferOffset) {
    const dst = this.getChannelData(channel);
    const off = bufferOffset || 0;
    const n = Math.min(src.length, dst.length - off);
    for (let i = 0; i < n; i++) dst[off + i] = src[i];
  }
};
Object.defineProperty(AudioBuffer.prototype, Symbol.toStringTag, { value: 'AudioBuffer', configurable: true });
_markNative(AudioBuffer);
_markNativeProto(AudioBuffer.prototype);
// The context classes are (re)defined below — sweep their prototypes only
// after the definitions exist, otherwise snapshot creation crashes on
// `undefined.prototype`.
globalThis.BaseAudioContext = class BaseAudioContext {
  constructor() { this.sampleRate=_fp('audioSampleRate'); this.state='running'; this.currentTime=0; this.baseLatency=_fp('audioBaseLatency'); this.outputLatency=0;
    this.destination=_audioNode(this, {maxChannelCount:2,numberOfInputs:1,numberOfOutputs:0,channelCount:2});
    this.listener={positionX:_audioParam(0),positionY:_audioParam(0),positionZ:_audioParam(0),forwardX:_audioParam(0),forwardY:_audioParam(0),forwardZ:_audioParam(-1),upX:_audioParam(0),upY:_audioParam(1),upZ:_audioParam(0),setPosition(){},setOrientation(){}}; }
  // BaseAudioContext is an EventTarget (detectors addEventListener("complete")
  // on OfflineAudioContext).
  addEventListener(t, f) { if (typeof f === 'function') { const l = this._ls || (this._ls = {}); (l[t] || (l[t] = [])).push(f); } }
  removeEventListener(t, f) { const l = this._ls && this._ls[t]; if (l) { const i = l.indexOf(f); if (i >= 0) l.splice(i, 1); } }
  dispatchEvent(ev) { const l = (this._ls && this._ls[ev.type]) || []; for (const f of l.slice()) { try { f.call(this, ev); } catch (e) {} } return true; }
  createOscillator() { return _audioNode(this, {type:'sine',frequency:_audioParam(440),detune:_audioParam(0),start(){},stop(){},onended:null}); }
  createDynamicsCompressor() { return _audioNode(this, {threshold:_audioParam(_fp('compThreshold'),-100,0),knee:_audioParam(_fp('compKnee'),0,40),ratio:_audioParam(_fp('compRatio'),1,20),attack:_audioParam(0.003,0,1),release:_audioParam(0.25,0,1),reduction:0}); }
  createAnalyser() {
    return _audioNode(this, {fftSize:2048,frequencyBinCount:1024,minDecibels:-100,maxDecibels:-30,smoothingTimeConstant:0.8,
      getByteFrequencyData(a){for(let i=0;i<a.length;i++)a[i]=Math.floor(_fpRand(600+i)*10);},
      getFloatFrequencyData(a){for(let i=0;i<a.length;i++)a[i]=-100+_fpRand(700+i)*5;},
      getByteTimeDomainData(a){for(let i=0;i<a.length;i++)a[i]=128;},
      getFloatTimeDomainData(a){for(let i=0;i<a.length;i++)a[i]=0;}
    });
  }
  createGain() { return _audioNode(this, {gain:_audioParam(1)}); }
  createBiquadFilter() { return _audioNode(this, {type:'lowpass',frequency:_audioParam(350,0,24000),Q:_audioParam(1),detune:_audioParam(0),gain:_audioParam(0,-40,40),
    getFrequencyResponse(){}}); }
  createBufferSource() { return _audioNode(this, {buffer:null,playbackRate:_audioParam(1),detune:_audioParam(0),loop:false,loopStart:0,loopEnd:0,start(){},stop(){},onended:null}); }
  createBuffer(ch,len,rate) { return new AudioBuffer({numberOfChannels:ch,length:len,sampleRate:rate||this.sampleRate}); }
  createScriptProcessor() { return _audioNode(this, {onaudioprocess:null,bufferSize:4096}); }
  createChannelMerger(n) { return _audioNode(this, {numberOfInputs:n||6,numberOfOutputs:1}); }
  createChannelSplitter(n) { return _audioNode(this, {numberOfInputs:1,numberOfOutputs:n||6}); }
  createDelay() { return _audioNode(this, {delayTime:_audioParam(0,0,1)}); }
  createWaveShaper() { return _audioNode(this, {curve:null,oversample:'none'}); }
  decodeAudioData(buf) { return Promise.resolve(this.createBuffer(2,44100,44100)); }
  resume() { this.state='running'; return Promise.resolve(); }
  suspend() { this.state='suspended'; return Promise.resolve(); }
};
globalThis.AudioContext = class AudioContext extends BaseAudioContext {
  close() { this.state='closed'; return Promise.resolve(); }
  createMediaStreamDestination() { return _audioNode(this, {stream:{getTracks(){return [];},getAudioTracks(){return [];}}}); }
  createMediaElementSource(el) { return _audioNode(this, {mediaElement:el||null}); }
  getOutputTimestamp() { return {contextTime:this.currentTime,performanceTime:0}; }
};
globalThis.OfflineAudioContext = class OfflineAudioContext extends BaseAudioContext {
  constructor(ch,len,rate) { super(); this.sampleRate=rate||44100; this.length=len||44100; this.numberOfChannels=ch||1; }
  startRendering() {
    const buf = this.createBuffer(this.numberOfChannels,this.length,this.sampleRate);
    // Deterministic, identity-seeded pseudo-signal: an all-zero render reads as
    // "audio is fake" to detectors; real Chrome yields a compressor-shaped
    // waveform. Stable per fingerprint seed, nonzero, consistent between
    // getChannelData and copyFromChannel.
    const d = buf.getChannelData(0);
    for (let i = 0; i < d.length; i++) {
      d[i] = (_fpRand(8100 + i) * 2 - 1) * 0.1 * Math.exp(-i / (d.length * 0.7));
    }
    const p = Promise.resolve(buf);
    // Real implementations fire 'complete' with the rendered buffer.
    p.then(() => {
      const ev = { type: 'complete', renderedBuffer: buf, target: this };
      if (typeof this.oncomplete === 'function') { try { this.oncomplete(ev); } catch (e) {} }
      this.dispatchEvent(ev);
    });
    return p;
  }
};
globalThis.webkitAudioContext = globalThis.AudioContext;
globalThis.webkitOfflineAudioContext = globalThis.OfflineAudioContext;
_markNativeProto(globalThis.BaseAudioContext.prototype);
_markNativeProto(globalThis.AudioContext.prototype);
_markNativeProto(globalThis.OfflineAudioContext.prototype);

globalThis.speechSynthesis = {
  speaking: false, pending: false, paused: false,
  getVoices() {
    // Real macOS Chrome exposes ~50 local system voices plus ~25 Google
    // network voices across dozens of languages. A single synthetic voice
    // is a fingerprint outlier (Castle voices collector).
    if (!this._voices) {
      const mk = (name, lang, local, def) => ({ name, lang, default: !!def, localService: !!local, voiceURI: name });
      this._voices = [
        mk('Alex', 'en-US', true, true), mk('Ava', 'en-US', true), mk('Ayumi', 'ja-JP', true),
        mk('Bad News', 'en-US', true), mk('Bahh', 'en-US', true), mk('Bells', 'en-US', true),
        mk('Boing', 'en-US', true), mk('Bruce', 'en-US', true), mk('Bubbles', 'en-US', true),
        mk('Carmit', 'he-IL', true), mk('Cellos', 'en-US', true), mk('Chan-yu', 'zh-TW', true),
        mk('Damayanti', 'id-ID', true), mk('Daniel', 'en-GB', true), mk('Deranged', 'en-US', true),
        mk('Diego', 'es-AR', true), mk('Ellen', 'nl-BE', true), mk('Fiona', 'en-GB', true),
        mk('Fred', 'en-US', true), mk('Good News', 'en-US', true), mk('Hesham', 'ar-SA', true),
        mk('Jester', 'en-US', true), mk('Joana', 'pt-PT', true), mk('Junior', 'en-US', true),
        mk('Kanya', 'th-TH', true), mk('Karen', 'en-AU', true), mk('Kathy', 'en-US', true),
        mk('Katja', 'de-DE', true), mk('Kyoko', 'ja-JP', true), mk('Laura', 'sk-SK', true),
        mk('Lekha', 'hi-IN', true), mk('Luciana', 'pt-BR', true), mk('Maged', 'ar-SA', true),
        mk('Mariska', 'hu-HU', true), mk('Mei-Jia', 'zh-TW', true), mk('Melina', 'el-GR', true),
        mk('Milena', 'ru-RU', true), mk('Moira', 'en-IE', true), mk('Monica', 'es-ES', true),
        mk('Nora', 'nb-NO', true), mk('Organ', 'en-US', true), mk('Paulina', 'es-MX', true),
        mk('Ralph', 'en-US', true), mk('Samantha', 'en-US', true), mk('Sara', 'da-DK', true),
        mk('Satu', 'fi-FI', true), mk('Sin-ji', 'zh-HK', true), mk('Tessa', 'en-GB', true),
        mk('Thomas', 'fr-FR', true), mk('Ting-Ting', 'zh-CN', true), mk('Tomas', 'cs-CZ', true),
        mk('Trinoids', 'en-US', true), mk('Veena', 'en-IN', true), mk('Victoria', 'en-US', true),
        mk('Whisper', 'en-US', true), mk('Xander', 'nl-NL', true), mk('Yelda', 'tr-TR', true),
        mk('Yuna', 'ko-KR', true), mk('Zosia', 'pl-PL', true), mk('Zuzana', 'cs-CZ', true),
        mk('Google US English', 'en-US', false), mk('Google UK English Female', 'en-GB', false),
        mk('Google UK English Male', 'en-GB', false), mk('Google Deutsch', 'de-DE', false),
        mk('Google español', 'es-ES', false), mk('Google français', 'fr-FR', false),
        mk('Google हिन्दी', 'hi-IN', false), mk('Google Bahasa Indonesia', 'id-ID', false),
        mk('Google italiano', 'it-IT', false), mk('Google 日本語', 'ja-JP', false),
        mk('Google 한국의', 'ko-KR', false), mk('Google Nederlands', 'nl-NL', false),
        mk('Google polski', 'pl-PL', false), mk('Google português do Brasil', 'pt-BR', false),
        mk('Google русский', 'ru-RU', false), mk('Google 普通话（中国大陆）', 'zh-CN', false),
        mk('Google 粤語（香港）', 'zh-HK', false), mk('Google闽南话（台湾）', 'zh-TW', false),
      ];
    }
    return this._voices;
  },
  speak() {}, cancel() {}, pause() {}, resume() {},
  addEventListener() {}, removeEventListener() {},
  onvoiceschanged: null,
};
globalThis.SpeechSynthesisUtterance = class SpeechSynthesisUtterance { constructor(t){this.text=t;this.lang='en-US';this.rate=1;this.pitch=1;this.volume=1;} };

globalThis.MediaStream = class MediaStream { constructor(){this.id='';this.active=true;} getTracks(){return [];} getAudioTracks(){return [];} getVideoTracks(){return [];} addTrack(){} removeTrack(){} clone(){return new MediaStream();} };
globalThis.MediaStreamTrack = class MediaStreamTrack { constructor(){this.kind='';this.enabled=true;this.readyState='live';} stop(){} clone(){return new MediaStreamTrack();} };
globalThis.RTCPeerConnection = class RTCPeerConnection {
  constructor(){this.localDescription=null;this.remoteDescription=null;this.iceConnectionState='new';this.iceGatheringState='new';this.signalingState='stable';this.connectionState='new';}
  createOffer(){return Promise.resolve({type:'offer',sdp:''});}
  createAnswer(){return Promise.resolve({type:'answer',sdp:''});}
  setLocalDescription(){return Promise.resolve();}
  setRemoteDescription(){return Promise.resolve();}
  addIceCandidate(){return Promise.resolve();}
  close(){}
  createDataChannel(){return {close(){},send(){},addEventListener(){},removeEventListener(){}};}
  addEventListener(){} removeEventListener(){}
  getStats(){return Promise.resolve(new Map());}
};
globalThis.RTCSessionDescription = class RTCSessionDescription { constructor(d){this.type=d?.type;this.sdp=d?.sdp;} };
globalThis.RTCIceCandidate = class RTCIceCandidate { constructor(d){this.candidate=d?.candidate||'';} };

// Minimal but spec-shape-correct IndexedDB shim. We don't persist anything,
// but authentication libraries (Firebase, Supabase, dexie) hang forever on
// the first `get` because their request's `onsuccess` is never called. Fire
// `onsuccess` asynchronously with `null` so reads complete-but-empty, which
// most libraries treat as a cache miss and fall back to the network.
function _idbRequest(produceResult) {
  const req = {
    result: undefined,
    error: null,
    source: null,
    transaction: null,
    readyState: 'pending',
    onsuccess: null,
    onerror: null,
    addEventListener(type, fn) { req['on' + type] = fn; },
    removeEventListener(type, fn) { if (req['on' + type] === fn) req['on' + type] = null; },
  };
  Promise.resolve().then(() => {
    try {
      req.result = produceResult();
      req.readyState = 'done';
      if (typeof req.onsuccess === 'function') {
        try { req.onsuccess({ target: req, type: 'success' }); } catch (e) {}
      }
    } catch (e) {
      req.error = e; req.readyState = 'done';
      if (typeof req.onerror === 'function') {
        try { req.onerror({ target: req, type: 'error' }); } catch (e2) {}
      }
    }
  });
  return req;
}

function _idbObjectStore(name) {
  const data = new Map();
  return {
    name,
    keyPath: null,
    autoIncrement: false,
    indexNames: { contains() { return false; }, length: 0, item() { return null; } },
    transaction: null,
    add(value, key) { const k = key ?? Date.now(); data.set(k, value); return _idbRequest(() => k); },
    put(value, key) { const k = key ?? Date.now(); data.set(k, value); return _idbRequest(() => k); },
    get(key) { return _idbRequest(() => data.get(key) ?? undefined); },
    getAll() { return _idbRequest(() => Array.from(data.values())); },
    getAllKeys() { return _idbRequest(() => Array.from(data.keys())); },
    getKey(key) { return _idbRequest(() => (data.has(key) ? key : undefined)); },
    delete(key) { return _idbRequest(() => { data.delete(key); return undefined; }); },
    clear() { return _idbRequest(() => { data.clear(); return undefined; }); },
    count() { return _idbRequest(() => data.size); },
    openCursor() { return _idbRequest(() => null); },
    openKeyCursor() { return _idbRequest(() => null); },
    createIndex() { return { name: '', keyPath: '', unique: false, multiEntry: false, get() { return _idbRequest(() => undefined); } }; },
    index() { return { get() { return _idbRequest(() => undefined); }, getAll() { return _idbRequest(() => []); }, count() { return _idbRequest(() => 0); }, openCursor() { return _idbRequest(() => null); } }; },
    deleteIndex() {},
  };
}

function _idbTransaction(storeNames) {
  const stores = new Map();
  const names = Array.isArray(storeNames) ? storeNames : [storeNames];
  for (const n of names) stores.set(String(n), _idbObjectStore(String(n)));
  const tx = {
    db: null,
    mode: 'readonly',
    objectStoreNames: { contains: (n) => stores.has(String(n)), length: stores.size },
    onabort: null, oncomplete: null, onerror: null,
    error: null,
    objectStore(name) {
      let s = stores.get(name);
      if (!s) { s = _idbObjectStore(name); stores.set(name, s); }
      s.transaction = tx;
      return s;
    },
    abort() {},
    commit() {},
    addEventListener(type, fn) { tx['on' + type] = fn; },
    removeEventListener(type, fn) { if (tx['on' + type] === fn) tx['on' + type] = null; },
  };
  Promise.resolve().then(() => {
    if (typeof tx.oncomplete === 'function') {
      try { tx.oncomplete({ target: tx, type: 'complete' }); } catch (e) {}
    }
  });
  return tx;
}

function _idbDatabase(name, version) {
  return {
    name,
    version,
    objectStoreNames: { contains() { return false; }, length: 0, item() { return null; } },
    createObjectStore(n) { return _idbObjectStore(n); },
    deleteObjectStore() {},
    transaction(storeNames, mode) {
      const tx = _idbTransaction(storeNames);
      tx.mode = mode || 'readonly';
      return tx;
    },
    close() {},
    onversionchange: null, onabort: null, onerror: null, onclose: null,
    addEventListener() {}, removeEventListener() {},
  };
}

globalThis.indexedDB = {
  open(name, version) {
    return _idbRequest(() => _idbDatabase(name, version || 1));
  },
  deleteDatabase(_name) { return _idbRequest(() => undefined); },
  databases() { return Promise.resolve([]); },
  cmp(a, b) { return a < b ? -1 : a > b ? 1 : 0; },
};
globalThis.IDBKeyRange = {
  only(v) { return { lower: v, upper: v, lowerOpen: false, upperOpen: false, includes(x) { return x === v; } }; },
  lowerBound(v, open) { return { lower: v, upper: null, lowerOpen: !!open, upperOpen: false, includes(x) { return open ? x > v : x >= v; } }; },
  upperBound(v, open) { return { lower: null, upper: v, lowerOpen: false, upperOpen: !!open, includes(x) { return open ? x < v : x <= v; } }; },
  bound(l, u, lo, uo) { return { lower: l, upper: u, lowerOpen: !!lo, upperOpen: !!uo, includes(x) { return (lo ? x > l : x >= l) && (uo ? x < u : x <= u); } }; },
};

globalThis.caches = {
  open() { return Promise.resolve({ match(){return Promise.resolve(undefined);}, put(){return Promise.resolve();}, delete(){return Promise.resolve(false);}, keys(){return Promise.resolve([]);} }); },
  match() { return Promise.resolve(undefined); },
  has() { return Promise.resolve(false); },
  delete() { return Promise.resolve(false); },
  keys() { return Promise.resolve([]); },
};

_markNative(BaseAudioContext); _markNative(AudioContext); _markNative(OfflineAudioContext);
_markNative(SpeechSynthesisUtterance);
_markNative(MediaStream); _markNative(MediaStreamTrack);
_markNative(RTCPeerConnection); _markNative(RTCSessionDescription); _markNative(RTCIceCandidate);

const _OrigDateTimeFormat = Intl.DateTimeFormat;
// Derive the reported timezone from the configured language so the persona
// stays coherent (zh-CN + Europe/Berlin — the old hardcoded default — is a
// mismatch risk engines notice).
// Accept-Language may arrive as a raw header ("zh-CN,zh;q=0.9,en;q=0.8").
// navigator.language must be a single BCP-47 tag and navigator.languages a
// tag list — a q-weighted string in either is a hard headless tell.
function __ditingLangList() {
  const raw = String(globalThis.__diting_lang || 'zh-CN');
  const tags = raw.split(',').map((s) => s.split(';')[0].trim()).filter(Boolean);
  const out = tags.length ? tags : ['zh-CN'];
  if (out.length === 1 && out[0].indexOf('-') !== -1) out.push(out[0].split('-')[0]);
  return out;
}
function __ditingTZFromLang() {
  const lang = __ditingLangList()[0].toLowerCase();
  const map = {
    'zh': 'Asia/Shanghai', 'ja': 'Asia/Tokyo', 'ko': 'Asia/Seoul',
    'en': 'America/New_York', 'de': 'Europe/Berlin', 'fr': 'Europe/Paris',
    'es': 'Europe/Madrid', 'it': 'Europe/Rome', 'ru': 'Europe/Moscow',
    'pt': 'Europe/Lisbon', 'nl': 'Europe/Amsterdam', 'tr': 'Europe/Istanbul',
  };
  for (const k of Object.keys(map)) {
    if (lang.startsWith(k)) return map[k];
  }
  return 'Asia/Shanghai';
}
// Intl's *default* locale comes from the process locale (V8/ICU follows
// LANG), not from the configured language — so on a host running under
// LANG=en_US, navigator.language says zh-CN while
// Intl.DateTimeFormat().resolvedOptions().locale says en-US. Same class of
// mismatch as the timezone one above, and an equally hard headless tell.
// The Rust side pins ICU's default for fresh isolates, but V8 caches the
// resolved default per-isolate after first Intl use, so the authoritative
// fix lives here: bind undefined/null locale args to the configured
// language. Explicit locales pass through untouched.
function __ditingLocaleArg(locales) {
  if (locales === undefined || locales === null) return __ditingLangList()[0];
  return locales;
}
// Wrapping a native constructor with a plain JS function is itself a
// fingerprint: Function.prototype.toString stops returning [native code],
// name/length drift (stock Intl constructors are all length 0), and
// prototype.constructor stops closing on Intl.X. A Proxy with only a
// construct trap forwards everything else to the native target — the spec
// resolves a callable proxy's source text from its target — so nothing has
// to be forged by hand. It also keeps "class constructor requires new":
// a plain wrapper is callable without new and silently returns an
// instance, the proxy's default apply forwards to the native and throws
// like stock.
// Registry backing the toString disguise below: wrapped proxy -> native
// original it stands in for.
const __ditingNativeOf = new Map();
// Shared per-call binding for the DateTimeFormat proxies.
function __ditingDTFArgs(args) {
  if (!args[1]) args[1] = {};
  if (!args[1].timeZone) args[1].timeZone = __ditingTZFromLang();
  args[0] = __ditingLocaleArg(args[0]);
  return args;
}
Intl.DateTimeFormat = new Proxy(_OrigDateTimeFormat, {
  construct(target, args, newTarget) {
    return Reflect.construct(target, __ditingDTFArgs(args), newTarget);
  },
  // ECMA-402 legacy: these constructors stay callable without new. The
  // proxy's default call forwarding resolves the legacy NewTarget to the
  // inner native, which skips the construct trap — so bind here too.
  apply(target, thisArg, args) {
    return Reflect.construct(target, __ditingDTFArgs(args), target);
  },
});
__ditingNativeOf.set(Intl.DateTimeFormat, _OrigDateTimeFormat);
Object.defineProperty(_OrigDateTimeFormat.prototype, 'constructor', {
  value: Intl.DateTimeFormat, writable: true, enumerable: false, configurable: true,
});
const _origResolved = _OrigDateTimeFormat.prototype.resolvedOptions;
_OrigDateTimeFormat.prototype.resolvedOptions = new Proxy(_origResolved, {
  apply(target, thisArg, args) {
    const r = Reflect.apply(target, thisArg, args);
    if (r.timeZone === 'UTC') r.timeZone = __ditingTZFromLang();
    return r;
  },
});
__ditingNativeOf.set(_OrigDateTimeFormat.prototype.resolvedOptions, _origResolved);
// The remaining (locales, options) Intl constructors get the same
// default-locale binding via the same proxy treatment.
(function() {
  const names = ['NumberFormat', 'Collator', 'PluralRules', 'RelativeTimeFormat',
                 'ListFormat', 'DisplayNames', 'Segmenter'];
  for (const n of names) {
    const Orig = Intl[n];
    if (typeof Orig !== 'function' || !Orig.prototype) continue;
    Intl[n] = new Proxy(Orig, {
      construct(target, args, newTarget) {
        args[0] = __ditingLocaleArg(args[0]);
        return Reflect.construct(target, args, newTarget);
      },
      apply(target, thisArg, args) {
        args[0] = __ditingLocaleArg(args[0]);
        return Reflect.construct(target, args, target);
      },
    });
    __ditingNativeOf.set(Intl[n], Orig);
    Object.defineProperty(Orig.prototype, 'constructor', {
      value: Intl[n], writable: true, enumerable: false, configurable: true,
    });
  }
})();
// V8 renders Function.prototype.toString on a proxy over a native as the
// anonymous "function () { [native code] }" — stock shows the name
// ("function NumberFormat() { [native code] }"). Close that last gap by
// routing toString through a proxy of the real Function.prototype.toString
// that answers for the wrapped set from the original and reflects
// everything else untouched. This covers both Intl.X.toString() and a
// direct Function.prototype.toString.call(Intl.X); the disguise itself
// still renders native because the spec resolves a callable proxy's source
// text from its target.
const __ditingFnToString = Function.prototype.toString;
Function.prototype.toString = new Proxy(__ditingFnToString, {
  apply(target, thisArg, args) {
    if (__ditingNativeOf.has(thisArg)) {
      return __ditingFnToString.call(__ditingNativeOf.get(thisArg));
    }
    return Reflect.apply(target, thisArg, args);
  },
});

if (typeof PointerEvent === 'undefined') {
  globalThis.PointerEvent = class PointerEvent extends MouseEvent {
    constructor(type, opts={}) { super(type, opts); this.pointerId = opts.pointerId || 0; this.width = opts.width || 1; this.height = opts.height || 1; this.pressure = opts.pressure || 0; this.pointerType = opts.pointerType || 'mouse'; }
  };
}

if (typeof navigator.credentials === 'undefined') {
  navigator.credentials = { get(){return Promise.resolve(null);}, create(){return Promise.resolve(null);}, store(){return Promise.resolve();}, preventSilentAccess(){return Promise.resolve();} };
}

globalThis.opener = null;

globalThis.Worker = class Worker {
  constructor(url, options) {
    this.onmessage = null;
    this.onerror = null;
    this._terminated = false;
    this._listeners = {};
    this._code = null;
    this._codeReady = false;
    this._pending = null;
    this._workerSelf = null;
    const worker = this;

    // Resolve the source: blob store (text), blob object reference
    // (readable immediately — Blob.text resolves as a microtask, before
    // any setTimeout(0) macrotask), or fetch for http(s) URLs — INCLUDING
    // RELATIVE ones like "/workers/signals-worker.js", which WorkOS Radar
    // and many SDKs use. The old regex only accepted absolute http(s),
    // so relative-script workers silently resolved to null and the page
    // timed out waiting for onmessage.
    const resolveCode = () => {
      if (typeof url !== 'string') return Promise.resolve(null);
      const text = globalThis.__blobStore?.[url];
      if (text !== undefined) return Promise.resolve(text);
      const obj = globalThis.__blobObjs?.[url];
      if (obj && typeof obj.text === 'function') return obj.text();
      let abs = url;
      if (!/^(https?|blob|data):/i.test(url)) {
        try { abs = new URL(url, globalThis.location?.href || 'https://example.com/').href; }
        catch { return Promise.resolve(null); }
      }
      if (/^https?:/i.test(abs)) {
        return fetch(abs).then(r => r.text()).catch(e => { worker._fetchError = e; return null; });
      }
      return Promise.resolve(null);
    };
    resolveCode().then(code => {
      worker._code = code;
      worker._codeReady = true;
      // Drain messages that queued while the source loaded. Delivery here
      // runs as a microtask at the next JS yield - immune to macro-task
      // starvation. A busy event loop (Next.js hydration executing dozens
      // of chunks) can starve setTimeout retries for many seconds, long
      // past a collector's 5s worker-response window (timings.worker was
      // 10.9s on a real authk load before this).
      if (worker._pending && worker._pending.length) {
        const m = worker._pending.shift();
        if (m && !m.delivered) {
          m.delivered = true;
          if (code) runWorkerMessage(worker, m.data);
          else fireWorkerError(worker, worker._fetchError || new Error('Worker script failed to load or execute (no message from browser)'), worker._listeners);
        }
      }
    });
  }
  postMessage(data) {
    if (this._terminated) return;
    const worker = this;
    const msg = { data, delivered: false };
    if (!this._codeReady) (this._pending = this._pending || []).push(msg);
    // Timer fallback for the case where the source resolved before this
    // message arrived. Retry until the source resolves - deadline-based,
    // not a fixed attempt count: a real network fetch of the worker script
    // can take far longer than a small retry budget, and a worker whose
    // message was dropped is indistinguishable from a dead one to the page.
    const deadline = Date.now() + 10000;
    const attempt = () => {
      if (worker._terminated || msg.delivered) return;
      if (!worker._codeReady) {
        if (Date.now() < deadline) setTimeout(attempt, 8);
        return;
      }
      msg.delivered = true;
      if (!worker._code) {
        const err = worker._fetchError || new Error('Worker script failed to load or execute (no message from browser)');
        fireWorkerError(worker, err, worker._listeners);
        return;
      }
      runWorkerMessage(worker, data);
    };
    setTimeout(attempt, 0);
  }
  terminate() { this._terminated = true; }
  addEventListener(type, fn) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(fn);
  }
  removeEventListener(type, fn) {
    if (this._listeners[type]) this._listeners[type] = this._listeners[type].filter(h => h !== fn);
  }
};
function runWorkerMessage(worker, data) {
  try {
    if (!worker._workerSelf) {
        // WorkerGlobalScope-shaped `self`: real workers expose ~40 props
        // and NO window/document. Parameter shadowing in the Function
        // wrapper keeps the page realm's DOM out of the worker's scope,
        // so `typeof window` inside the worker reads "undefined".
        const workerSelf = {
          onmessage: null,
          postMessage: (msg) => {
            const evt = { data: msg };
            if (worker.onmessage) worker.onmessage(evt);
            const handlers = worker._listeners['message'] || [];
            for (const h of handlers) h(evt);
          },
          addEventListener: (type, fn) => { workerSelf['on' + type] = fn; },
          removeEventListener: () => {},
          close: () => { worker._terminated = true; },
          crypto: globalThis.crypto,
          TextEncoder: globalThis.TextEncoder,
          TextDecoder: globalThis.TextDecoder,
          atob: globalThis.atob,
          btoa: globalThis.btoa,
          setTimeout: globalThis.setTimeout,
          setInterval: globalThis.setInterval,
          clearTimeout: globalThis.clearTimeout,
          clearInterval: globalThis.clearInterval,
          fetch: globalThis.fetch,
          console: globalThis.console,
          performance: { now: () => Date.now(), timeOrigin: globalThis.performance?.timeOrigin || 0 },
          location: { href: (globalThis.location && globalThis.location.href) || 'https://example.com/', origin: (globalThis.location && globalThis.location.origin) || 'https://example.com', protocol: 'https:', host: (globalThis.location && globalThis.location.host) || 'example.com', hostname: (globalThis.location && globalThis.location.hostname) || 'example.com', port: '', pathname: '/', search: '', hash: '', toString() { return this.href; } },
          // Worker Navigator must mirror the page persona: collectors
          // cross-check platform/userAgent against the main-thread values.
          navigator: { hardwareConcurrency: globalThis.navigator.hardwareConcurrency, userAgent: globalThis.navigator.userAgent, appVersion: globalThis.navigator.appVersion, platform: globalThis.navigator.platform, language: globalThis.navigator.language, languages: globalThis.navigator.languages, deviceMemory: globalThis.navigator.deviceMemory, onLine: true },
          Request: globalThis.Request, Response: globalThis.Response,
          Headers: globalThis.Headers, Blob: globalThis.Blob,
          FormData: globalThis.FormData, URL: globalThis.URL,
          URLSearchParams: globalThis.URLSearchParams,
          AbortController: globalThis.AbortController, AbortSignal: globalThis.AbortSignal,
          WebSocket: globalThis.WebSocket, Event: globalThis.Event,
          MessageEvent: globalThis.MessageEvent, ErrorEvent: globalThis.ErrorEvent,
          indexedDB: { open: () => ({ onsuccess: null, onerror: null, onupgradeneeded: null }) },
          // Worker scope must expose OffscreenCanvas too: WorkOS Radar's
          // signals-worker runs its WebGL fingerprint collector in the worker
          // and crashed with `d.OffscreenCanvas is not a constructor` when
          // missing (which then wedged the whole session's dispatch).
          OffscreenCanvas: globalThis.OffscreenCanvas,
          ImageBitmap: globalThis.ImageBitmap,
          createImageBitmap: globalThis.createImageBitmap,
          caches: { open: () => Promise.reject(new DOMException('NotFoundError')), keys: () => Promise.resolve([]) },
          isSecureContext: true,
          origin: (globalThis.location && globalThis.location.origin) || 'https://example.com',
          importScripts: () => {},
          queueMicrotask: globalThis.queueMicrotask,
          structuredClone: globalThis.structuredClone,
        };
        workerSelf.self = workerSelf;
        worker._workerSelf = workerSelf;
        const fn = new Function('self', 'postMessage', 'addEventListener', 'removeEventListener', 'close', 'window', 'document', 'navigator', 'location', 'importScripts',
          worker._code);
        fn(workerSelf, workerSelf.postMessage, workerSelf.addEventListener, workerSelf.removeEventListener, workerSelf.close, undefined, undefined, workerSelf.navigator, workerSelf.location, workerSelf.importScripts);
    }
    if (worker._workerSelf.onmessage) worker._workerSelf.onmessage({ data });
  } catch(e) {
    console.error('Worker error:', e.message);
    fireWorkerError(worker, e, worker._listeners);
  }
}

// Module-level so both construction-time fetch failures and execution-time
// throws share one ErrorEvent-shaped path.
function fireWorkerError(worker, err, listeners) {
  const message = err instanceof Error ? (err.message || err.name) : String(err);
  const evt = { message, filename: '', lineno: 0, colno: 0, error: err };
  const handlers = ((listeners && listeners['error']) || []).slice();
  if (worker.onerror) handlers.unshift(worker.onerror);
  for (const h of handlers) { try { h(evt); } catch {} }
}

globalThis.__blobStore = globalThis.__blobStore || {};
// Blob objects by URL, registered synchronously — Worker construction reads
// these so the createObjectURL → new Worker race can't lose.
globalThis.__blobObjs = globalThis.__blobObjs || {};
const _origCreateObjectURL = URL.createObjectURL;
URL.createObjectURL = function(blob) {
  if (blob && typeof blob.text === 'function') {
    const id = 'blob:obscura/' + Math.random().toString(36).substring(2);
    globalThis.__blobObjs[id] = blob;
    blob.text().then(text => { globalThis.__blobStore[id] = text; });
    return id;
  }
  return 'blob:obscura/fallback';
};
URL.revokeObjectURL = function(url) {
  delete globalThis.__blobStore[url];
};

// Window-level scrolling (upstream #468). The dominant infinite-scroll idiom
// is window-level — window.scrollTo(0, body.scrollHeight), window.scrollBy(0,
// 500), then a window 'scroll' listener — and no-op stubs meant the offset
// never moved, the sentinel never tripped, and automation that scrolled to
// trigger loading silently got one screenful.
//
// The page offset is stored on the scrolling element rather than in separate
// window state, so window.scrollY and document.scrollingElement.scrollTop are
// two views of one value, which is what pages assume. As with elements there
// is no layout, so the offset cannot be clamped to a real maximum.
function _scrollRoot() {
  const doc = globalThis.document;
  return (doc && doc.scrollingElement) || null;
}
function _windowScroll(x, y, relative) {
  const root = _scrollRoot();
  if (!root) return;
  const beforeLeft = root.scrollLeft || 0;
  const beforeTop = root.scrollTop || 0;
  let left, top;
  if (x !== null && typeof x === 'object') { left = x.left; top = x.top; }
  else { left = x; top = y; }
  if (left !== undefined) {
    root.scrollLeft = (relative ? (root.scrollLeft || 0) : 0) + (+left || 0);
  }
  if (top !== undefined) {
    root.scrollTop = (relative ? (root.scrollTop || 0) : 0) + (+top || 0);
  }
  if ((root.scrollLeft || 0) === beforeLeft && (root.scrollTop || 0) === beforeTop) {
    return;
  }
  // Async, matching the element path. Dispatched at the document AND the
  // window: a page scroll event reaches both in Chrome, but
  // Document.dispatchEvent here runs only its own listeners and does not
  // propagate, so firing once would strand half the listeners.
  setTimeout(() => {
    try {
      const doc = globalThis.document;
      if (doc) doc.dispatchEvent(new Event('scroll', { bubbles: false }));
      globalThis.dispatchEvent(new Event('scroll', { bubbles: false }));
    } catch (e) {}
  }, 0);
}
globalThis.scrollTo = function(x, y) { _windowScroll(x, y, false); };
globalThis.scrollBy = function(x, y) { _windowScroll(x, y, true); };
globalThis.scroll = function(x, y) { _windowScroll(x, y, false); };
_markNative(globalThis.scrollTo);
_markNative(globalThis.scrollBy);
_markNative(globalThis.scroll);
// Read-only accessors, as on a real Window: assigning window.scrollY does not
// scroll the page. These replace the hard-coded 0 data properties defined
// earlier, so they must stay after them.
for (const [name, offset] of [
  ['scrollX', 'scrollLeft'], ['pageXOffset', 'scrollLeft'],
  ['scrollY', 'scrollTop'], ['pageYOffset', 'scrollTop'],
]) {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    enumerable: true,
    get() { const root = _scrollRoot(); return root ? (root[offset] || 0) : 0; },
  });
}
globalThis.focus = function() {};
globalThis.blur = function() {};
globalThis.print = function() {};
globalThis.alert = function() {};
globalThis.confirm = function() { return true; };
globalThis.prompt = function() { return null; };
globalThis.open = function() { return null; };
globalThis.close = function() {};
globalThis.stop = function() {};
globalThis.postMessage = function(message, targetOrigin) {
  // Self-targeted postMessage (spec: window.postMessage to itself). Honor
  // the targetOrigin gate like the iframe path: '*'/'/'/matching origin
  // delivers; anything else drops silently (#704).
  let t = targetOrigin;
  if (t === undefined || t === null) t = '*';
  if (typeof t !== 'string' || t === '') return;
  if (t !== '*' && t !== '/') {
    const selfOrigin = (function() { try { return new URL(globalThis.location?.href || 'about:blank').origin; } catch(e) { return ''; } })();
    let tOrigin = '';
    try { tOrigin = new URL(t).origin; } catch(e) {}
    if (tOrigin === '' || tOrigin !== selfOrigin) return;
  }
  const event = new MessageEvent('message', {
    data: message,
    origin: (function() { try { return new URL(globalThis.location?.href || 'about:blank').origin; } catch(e) { return ''; } })(),
    source: globalThis,
  });
  Promise.resolve().then(() => { globalThis.dispatchEvent?.(event); });
};
globalThis.requestIdleCallback = globalThis.requestIdleCallback || function(cb) { return setTimeout(cb, 0); };
globalThis.cancelIdleCallback = globalThis.cancelIdleCallback || function(id) { clearTimeout(id); };
if (typeof ReadableStream === 'undefined') {
  globalThis.ReadableStream = class ReadableStream {
    constructor(source = {}, strategy = {}) {
      this._source = source; this._queue = []; this._closed = false; this._errored = null;
      this.locked = false; this._pullBusy = false; this._pendingReads = [];
      const stream = this;
      this._controller = {
        enqueue: (chunk) => {
          if (stream._closed) return;
          // A parked read gets the chunk directly; otherwise buffer it.
          if (stream._pendingReads.length > 0) {
            stream._pendingReads.shift().resolve({ value: chunk, done: false });
          } else {
            stream._queue.push(chunk);
          }
        },
        close: () => {
          if (stream._closed) return;
          stream._closed = true;
          const waiters = stream._pendingReads.splice(0);
          for (const w of waiters) w.resolve({ value: undefined, done: true });
        },
        error: (e) => {
          if (stream._closed) return;
          stream._errored = e || new Error("stream error");
          stream._closed = true;
          const waiters = stream._pendingReads.splice(0);
          for (const w of waiters) w.reject(stream._errored);
        },
        get desiredSize() { return 1; },
      };
      if (source.start) {
        try { source.start(this._controller); } catch (e) { this._controller.error(e); }
      }
    }
    getReader() {
      this.locked = true;
      const stream = this;
      return {
        read() {
          if (stream._queue.length > 0) return Promise.resolve({ value: stream._queue.shift(), done: false });
          if (stream._closed) {
            if (stream._errored) return Promise.reject(stream._errored);
            return Promise.resolve({ value: undefined, done: true });
          }
          // Pull-driven source (e.g. React's partial-flight re-wrapping
          // `new ReadableStream({async pull(controller){...}})`): ask the
          // source for a chunk, then hand back whatever it enqueued.
          if (stream._source.pull && !stream._pullBusy) {
            stream._pullBusy = true;
            return Promise.resolve()
              .then(() => stream._source.pull(stream._controller))
              .then(() => {
                stream._pullBusy = false;
                if (stream._queue.length > 0) return { value: stream._queue.shift(), done: false };
                if (stream._errored) throw stream._errored;
                return { value: undefined, done: true };
              }, (e) => {
                stream._pullBusy = false;
                stream._controller.error(e);
                throw stream._errored;
              });
          }
          // Live producer (a TransformStream's readable side, or any source
          // that enqueues after read): park the read until
          // enqueue/close/error wakes it.
          return new Promise((resolve, reject) => {
            stream._pendingReads.push({ resolve, reject });
          });
        },
        releaseLock() { stream.locked = false; },
        cancel() {
          stream._closed = true;
          if (stream._source.cancel) { try { stream._source.cancel(); } catch (e) {} }
          return Promise.resolve();
        },
        get closed() { return stream._closed ? Promise.resolve() : new Promise(() => {}); },
      };
    }
    cancel() { this._closed = true; return Promise.resolve(); }
    async pipeTo(dest) {
      const reader = this.getReader();
      const writer = dest.getWriter();
      try {
        for (;;) {
          const r = await reader.read();
          if (r.done) break;
          await writer.write(r.value);
        }
        await writer.close();
      } catch (e) {
        try { await writer.abort(e); } catch (_) {}
        throw e;
      } finally {
        try { reader.releaseLock(); } catch (_) {}
        try { writer.releaseLock(); } catch (_) {}
      }
    }
    pipeThrough(transform) {
      const out = transform && transform.readable;
      if (!out) return new ReadableStream();
      this.pipeTo(transform.writable).catch(() => {});
      return out;
    }
    tee() {
      // Materialize what's buffered, then hand out two independent streams
      // over a copy — clone-style consumption. (Interleaved tee of a live
      // pull-driven source is not supported; no in-the-wild consumer we
      // care about does that.)
      const chunks = this._queue.slice();
      const wasClosed = this._closed;
      const wasErr = this._errored;
      const mk = () => new ReadableStream({
        start(c) { for (const ch of chunks) c.enqueue(ch); if (wasErr) c.error(wasErr); else if (wasClosed) c.close(); },
      });
      return [mk(), mk()];
    }
    [Symbol.asyncIterator]() {
      const reader = this.getReader();
      return { next: () => reader.read(), return: () => { reader.releaseLock(); return Promise.resolve({done:true}); } };
    }
  };
}
if (typeof WritableStream === 'undefined') {
  globalThis.WritableStream = class WritableStream {
    constructor(sink = {}) { this._sink = sink; this.locked = false; }
    getWriter() {
      this.locked = true;
      const stream = this;
      return {
        write(chunk) { if (stream._sink.write) stream._sink.write(chunk); return Promise.resolve(); },
        close() { if (stream._sink.close) stream._sink.close(); return Promise.resolve(); },
        abort() { return Promise.resolve(); },
        releaseLock() { stream.locked = false; },
        get ready() { return Promise.resolve(); },
        get closed() { return Promise.resolve(); },
        get desiredSize() { return 1; },
      };
    }
    close() { return Promise.resolve(); }
    abort() { return Promise.resolve(); }
  };
}
if (typeof TransformStream === 'undefined') {
  globalThis.TransformStream = class TransformStream {
    // Minimal but real: writes flow through `transformer.transform` (or pass
    // through untouched) into the readable side, so pipeThrough() doesn't
    // silently drop the payload the way an empty stub does.
    constructor(transformer = {}) {
      let rc = null;
      const controller = {
        enqueue: (c2) => { if (rc) rc.enqueue(c2); },
        close: () => { if (rc) rc.close(); },
        error: (e) => { if (rc) rc.error(e); },
      };
      this.readable = new ReadableStream({ start(c) { rc = c; } });
      this.writable = new WritableStream({
        write(chunk) {
          try {
            if (transformer.transform) {
              transformer.transform(chunk, controller);
            } else if (rc) rc.enqueue(chunk);
          } catch (e) { if (rc) rc.error(e); }
        },
        close() {
          try { if (transformer.flush) transformer.flush(controller); }
          catch (e) { if (rc) { rc.error(e); return; } }
          if (rc) rc.close();
        },
      });
    }
  };
}

if (!globalThis.crypto) globalThis.crypto = {};
if (!globalThis.crypto.subtle) {
  // Real WebCrypto for the secret-key algorithms sites actually use: HMAC,
  // AES-GCM/CBC/CTR, PBKDF2 and HKDF, plus raw/JWK-oct key handling. The crypto
  // itself runs in Rust ops (RustCrypto); this shim only marshals bytes and
  // normalizes algorithm parameters. Public-key algorithms (RSA*, ECDSA, ECDH)
  // and non-symmetric key formats (pkcs8/spki) are not implemented and throw
  // NotSupportedError rather than returning fake data.
  const keyMaterial = new WeakMap();

  class CryptoKey {
    constructor() { throw new TypeError("Illegal constructor"); }
    get [Symbol.toStringTag]() { return "CryptoKey"; }
  }
  function makeKey(type, extractable, algorithm, usages, bytes) {
    const k = Object.create(CryptoKey.prototype);
    Object.defineProperty(k, "type", { value: type, enumerable: true });
    Object.defineProperty(k, "extractable", { value: !!extractable, enumerable: true });
    Object.defineProperty(k, "algorithm", { value: algorithm, enumerable: true });
    Object.defineProperty(k, "usages", { value: Object.freeze((usages || []).slice()), enumerable: true });
    keyMaterial.set(k, bytes);
    return k;
  }
  function keyBytes(key) {
    if (!(key instanceof CryptoKey) || !keyMaterial.has(key)) {
      throw new DOMException("Argument is not a valid CryptoKey", "InvalidAccessError");
    }
    return keyMaterial.get(key);
  }

  const toBytes = (data) => {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    return new Uint8Array(data || []);
  };
  const bufferOf = (u8) => new Uint8Array(u8).buffer;

  const ALGO_CANON = {
    "AES-CTR": "AES-CTR", "AES-CBC": "AES-CBC", "AES-GCM": "AES-GCM", "AES-KW": "AES-KW",
    "HMAC": "HMAC", "PBKDF2": "PBKDF2", "HKDF": "HKDF",
    "RSASSA-PKCS1-V1_5": "RSASSA-PKCS1-v1_5", "RSA-PSS": "RSA-PSS", "RSA-OAEP": "RSA-OAEP",
    "ECDSA": "ECDSA", "ECDH": "ECDH",
  };
  function normalizeAlgo(algorithm) {
    const a = typeof algorithm === "string" ? { name: algorithm } : (algorithm || {});
    const upper = String(a.name || "").toUpperCase();
    const name = ALGO_CANON[upper] || upper;
    return Object.assign({}, a, { name });
  }
  // SubtleCrypto hashes for HMAC/PBKDF2/HKDF and digest (SHA-1/256/384/512).
  function normalizeHash(h) {
    const n = (typeof h === "string" ? h : (h && h.name) || "").toUpperCase().replace("_", "-");
    if (n === "SHA-1" || n === "SHA-256" || n === "SHA-384" || n === "SHA-512") return n;
    throw new DOMException("Unsupported hash algorithm: " + (h && (h.name || h)), "NotSupportedError");
  }
  const hashBlockSize = (hash) => (hash === "SHA-384" || hash === "SHA-512" ? 128 : 64);

  function b64urlToBytes(s) {
    s = String(s).replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }
  function bytesToB64url(bytes) {
    let bin = "";
    for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  // Run an op, converting a Rust-side failure (bad GCM tag, bad CBC padding)
  // into the OperationError the WebCrypto spec requires. DOMExceptions we raise
  // ourselves pass through unchanged.
  function runOp(fn) {
    try { return fn(); }
    catch (e) {
      if (e instanceof DOMException) throw e;
      throw new DOMException(String((e && e.message) || e), "OperationError");
    }
  }

  function keyAlgorithmFor(alg, bytes) {
    switch (alg.name) {
      case "HMAC":
        return { name: "HMAC", hash: { name: normalizeHash(alg.hash) }, length: bytes.length * 8 };
      case "AES-CTR": case "AES-CBC": case "AES-GCM": case "AES-KW":
        if (bytes.length !== 16 && bytes.length !== 24 && bytes.length !== 32) {
          throw new DOMException("AES key data must be 128, 192, or 256 bits", "DataError");
        }
        return { name: alg.name, length: bytes.length * 8 };
      case "PBKDF2": return { name: "PBKDF2" };
      case "HKDF": return { name: "HKDF" };
      default:
        throw new DOMException("Unsupported key algorithm: " + alg.name, "NotSupportedError");
    }
  }

  const subtle = {
    async digest(algorithm, data) {
      const name = (typeof algorithm === "string" ? algorithm : algorithm && algorithm.name || "").toUpperCase().replace("_", "-");
      if (name !== "SHA-1" && name !== "SHA-256" && name !== "SHA-384" && name !== "SHA-512" &&
          name !== "SHA-512/224" && name !== "SHA-512/256") {
        throw new DOMException("Unrecognized algorithm name", "NotSupportedError");
      }
      return bufferOf(_OPS.op_subtle_digest(name, toBytes(data)));
    },

    async importKey(format, keyData, algorithm, extractable, keyUsages) {
      const alg = normalizeAlgo(algorithm);
      let bytes;
      if (format === "raw") {
        bytes = toBytes(keyData);
      } else if (format === "jwk") {
        if (!keyData || keyData.kty !== "oct" || typeof keyData.k !== "string") {
          throw new DOMException("Only symmetric 'oct' JWK keys are supported", "NotSupportedError");
        }
        bytes = b64urlToBytes(keyData.k);
      } else {
        throw new DOMException("Only 'raw' and symmetric 'jwk' key formats are supported", "NotSupportedError");
      }
      return makeKey("secret", extractable, keyAlgorithmFor(alg, bytes), keyUsages, bytes);
    },

    async exportKey(format, key) {
      const bytes = keyBytes(key);
      if (!key.extractable) throw new DOMException("Key is not extractable", "InvalidAccessError");
      if (format === "raw") return bufferOf(bytes);
      if (format === "jwk") {
        const jwk = { kty: "oct", k: bytesToB64url(bytes), ext: key.extractable, key_ops: key.usages.slice() };
        if (key.algorithm.name && key.algorithm.name.indexOf("AES-") === 0) {
          jwk.alg = "A" + (bytes.length * 8) + key.algorithm.name.slice(4);
        } else if (key.algorithm.name === "HMAC") {
          jwk.alg = "HS" + key.algorithm.hash.name.slice(4);
        }
        return jwk;
      }
      throw new DOMException("Only 'raw' and 'jwk' export is supported", "NotSupportedError");
    },

    async generateKey(algorithm, extractable, keyUsages) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "HMAC") {
        const hash = normalizeHash(alg.hash);
        const len = alg.length ? Math.ceil(alg.length / 8) : hashBlockSize(hash);
        const bytes = _OPS.op_random_bytes(len);
        return makeKey("secret", extractable, { name: "HMAC", hash: { name: hash }, length: len * 8 }, keyUsages, bytes);
      }
      if (alg.name === "AES-CTR" || alg.name === "AES-CBC" || alg.name === "AES-GCM" || alg.name === "AES-KW") {
        if (alg.length !== 128 && alg.length !== 192 && alg.length !== 256) {
          throw new DOMException("AES key length must be 128, 192, or 256 bits", "OperationError");
        }
        const bytes = _OPS.op_random_bytes(alg.length / 8);
        return makeKey("secret", extractable, { name: alg.name, length: alg.length }, keyUsages, bytes);
      }
      throw new DOMException("generateKey does not support " + alg.name, "NotSupportedError");
    },

    async sign(algorithm, key, data) {
      const alg = normalizeAlgo(algorithm);
      const bytes = keyBytes(key);
      if (alg.name === "HMAC") {
        const hash = key.algorithm && key.algorithm.hash ? key.algorithm.hash.name : normalizeHash(alg.hash);
        return bufferOf(runOp(() => _OPS.op_subtle_hmac(hash, bytes, toBytes(data))));
      }
      throw new DOMException("sign does not support " + alg.name, "NotSupportedError");
    },

    async verify(algorithm, key, signature, data) {
      const alg = normalizeAlgo(algorithm);
      const bytes = keyBytes(key);
      if (alg.name === "HMAC") {
        const hash = key.algorithm && key.algorithm.hash ? key.algorithm.hash.name : normalizeHash(alg.hash);
        const mac = runOp(() => _OPS.op_subtle_hmac(hash, bytes, toBytes(data)));
        const sig = toBytes(signature);
        if (sig.length !== mac.length) return false;
        let diff = 0;
        for (let i = 0; i < mac.length; i++) diff |= mac[i] ^ sig[i];
        return diff === 0;
      }
      throw new DOMException("verify does not support " + alg.name, "NotSupportedError");
    },

    async encrypt(algorithm, key, data) { return aesCipher(true, algorithm, key, data); },
    async decrypt(algorithm, key, data) { return aesCipher(false, algorithm, key, data); },

    async deriveBits(algorithm, baseKey, length) {
      const alg = normalizeAlgo(algorithm);
      const bytes = keyBytes(baseKey);
      const lenBytes = Math.ceil((length || 0) / 8);
      if (alg.name === "PBKDF2") {
        const hash = normalizeHash(alg.hash);
        const salt = toBytes(alg.salt);
        const iterations = alg.iterations >>> 0;
        return bufferOf(runOp(() => _OPS.op_subtle_pbkdf2(hash, bytes, salt, iterations, lenBytes)));
      }
      if (alg.name === "HKDF") {
        const hash = normalizeHash(alg.hash);
        const salt = alg.salt != null ? toBytes(alg.salt) : new Uint8Array(0);
        const info = alg.info != null ? toBytes(alg.info) : new Uint8Array(0);
        return bufferOf(runOp(() => _OPS.op_subtle_hkdf(hash, bytes, salt, info, lenBytes)));
      }
      throw new DOMException("deriveBits does not support " + alg.name, "NotSupportedError");
    },

    async deriveKey(algorithm, baseKey, derivedKeyAlgorithm, extractable, keyUsages) {
      const dAlg = normalizeAlgo(derivedKeyAlgorithm);
      let bits;
      if (dAlg.name === "HMAC") {
        bits = dAlg.length || hashBlockSize(normalizeHash(dAlg.hash)) * 8;
      } else if (dAlg.name === "AES-CTR" || dAlg.name === "AES-CBC" || dAlg.name === "AES-GCM" || dAlg.name === "AES-KW") {
        bits = dAlg.length;
        if (bits !== 128 && bits !== 192 && bits !== 256) {
          throw new DOMException("AES key length must be 128, 192, or 256 bits", "OperationError");
        }
      } else {
        throw new DOMException("deriveKey does not support deriving " + dAlg.name, "NotSupportedError");
      }
      const derivedBits = await this.deriveBits(algorithm, baseKey, bits);
      return this.importKey("raw", derivedBits, derivedKeyAlgorithm, extractable, keyUsages);
    },

    async wrapKey(format, key, wrappingKey, wrapAlgorithm) {
      const exported = await this.exportKey(format, key);
      const bytes = format === "jwk"
        ? new TextEncoder().encode(JSON.stringify(exported))
        : new Uint8Array(exported);
      return this.encrypt(wrapAlgorithm, wrappingKey, bytes);
    },

    async unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, keyUsages) {
      const decrypted = await this.decrypt(unwrapAlgorithm, unwrappingKey, wrappedKey);
      const keyData = format === "jwk"
        ? JSON.parse(new TextDecoder().decode(new Uint8Array(decrypted)))
        : decrypted;
      return this.importKey(format, keyData, unwrappedKeyAlgorithm, extractable, keyUsages);
    },
  };

  function aesCipher(encrypt, algorithm, key, data) {
    const alg = normalizeAlgo(algorithm);
    const bytes = keyBytes(key);
    const input = toBytes(data);
    if (alg.name === "AES-GCM") {
      const iv = toBytes(alg.iv);
      const aad = alg.additionalData != null ? toBytes(alg.additionalData) : new Uint8Array(0);
      const tagLength = alg.tagLength == null ? 128 : alg.tagLength;
      if (tagLength !== 128) {
        throw new DOMException("Only a 128-bit AES-GCM tag length is supported", "NotSupportedError");
      }
      return bufferOf(runOp(() => _OPS.op_subtle_aes_gcm(encrypt, bytes, iv, aad, input)));
    }
    if (alg.name === "AES-CBC") {
      const iv = toBytes(alg.iv);
      return bufferOf(runOp(() => _OPS.op_subtle_aes_cbc(encrypt, bytes, iv, input)));
    }
    if (alg.name === "AES-CTR") {
      const counter = toBytes(alg.counter);
      const length = alg.length >>> 0;
      return bufferOf(runOp(() => _OPS.op_subtle_aes_ctr(bytes, counter, length, input)));
    }
    throw new DOMException((encrypt ? "encrypt" : "decrypt") + " does not support " + alg.name, "NotSupportedError");
  }

  globalThis.CryptoKey = CryptoKey;
  globalThis.SubtleCrypto = function SubtleCrypto() { throw new TypeError("Illegal constructor"); };
  Object.setPrototypeOf(subtle, globalThis.SubtleCrypto.prototype);
  globalThis.crypto.subtle = subtle;

  // A CryptoKey cloned via structuredClone or postMessage is a different
  // object, so the keyMaterial WeakMap lookup misses and crypto.subtle throws
  // "Argument is not a valid CryptoKey". Rebuild the (cloned) key through
  // makeKey so it re-enters the WeakMap and stays usable. `seen` is the clone
  // memo _structuredClone hands every hook; populate it so one key reached
  // twice in a graph clones to one shared object (upstream 8698afc).
  globalThis.__diting_clone_hooks = globalThis.__diting_clone_hooks || {};
  globalThis.__diting_clone_hooks["CryptoKey"] = function (src, seen) {
    if (seen && seen.has(src)) return seen.get(src);
    const copy = makeKey(src.type, src.extractable, src.algorithm, src.usages, keyBytes(src));
    if (seen) seen.set(src, copy);
    return copy;
  };
}

if (typeof DOMRect === 'undefined') {
  globalThis.DOMRect = class DOMRect {
    constructor(x=0,y=0,w=0,h=0) { this.x=x;this.y=y;this.width=w;this.height=h;this.top=y;this.right=x+w;this.bottom=y+h;this.left=x; }
    toJSON() { return {x:this.x,y:this.y,width:this.width,height:this.height,top:this.top,right:this.right,bottom:this.bottom,left:this.left}; }
    static fromRect(r={}) { return new DOMRect(r.x,r.y,r.width,r.height); }
  };
}
if (typeof DOMPoint === 'undefined') {
  globalThis.DOMPoint = class DOMPoint {
    constructor(x=0,y=0,z=0,w=1) { this.x=x;this.y=y;this.z=z;this.w=w; }
    static fromPoint(p={}) { return new DOMPoint(p.x,p.y,p.z,p.w); }
  };
}
if (typeof DOMMatrix === 'undefined') {
  globalThis.DOMMatrix = class DOMMatrix {
    constructor() { this.a=1;this.b=0;this.c=0;this.d=1;this.e=0;this.f=0;this.is2D=true;this.isIdentity=true; }
    static fromMatrix() { return new DOMMatrix(); }
    static fromFloat32Array() { return new DOMMatrix(); }
    static fromFloat64Array() { return new DOMMatrix(); }
    multiply() { return new DOMMatrix(); }
    inverse() { return new DOMMatrix(); }
    translate() { return new DOMMatrix(); }
    scale() { return new DOMMatrix(); }
    rotate() { return new DOMMatrix(); }
    transformPoint(p) { return new DOMPoint(p?.x||0,p?.y||0); }
  };
}

if (typeof Image === 'undefined') {
  // In a real browser `new Image()` is `document.createElement('img')`, i.e. a
  // full HTMLImageElement. A plain-class shim has no `.style`, so
  // `new Image().style` was `undefined` and libraries that touch it on a
  // detached image threw (upstream issue #350). Build a real element so
  // `.style`, attribute reflection, and event dispatch all come for free.
  const _imgSrcDesc = Object.getOwnPropertyDescriptor(globalThis.HTMLImageElement.prototype, 'src');
  globalThis.Image = function Image(width, height) {
    const img = document.createElement('img');
    img.onload = null; img.onerror = null;
    img.complete = false; img.naturalWidth = 0; img.naturalHeight = 0;
    img.width = width !== undefined ? (width >>> 0) : 0;
    img.height = height !== undefined ? (height >>> 0) : 0;
    // There is no real image decoder, so emulate a successful decode: assigning
    // `.src` flips `complete` and fires `load` on a later tick. Lazy loaders
    // and preloaders that create `new Image()`, set `.src`, and wait for
    // `onload` (or addEventListener('load')) would hang forever otherwise.
    // Anti-bot scripts (Booking.com, upstream #394) pre-define a
    // non-configurable own `src` on <img> elements; redefining it throws
    // "Cannot redefine property: src" and kills the constructor. Skip the
    // emulation then: a page that owns `src` is instrumenting loads itself.
    const ownSrc = Object.getOwnPropertyDescriptor(img, 'src');
    if (!ownSrc || ownSrc.configurable) {
      Object.defineProperty(img, 'src', {
        configurable: true, enumerable: true,
        get() { return _imgSrcDesc.get.call(img); },
        set(v) {
          _imgSrcDesc.set.call(img, v);
          if (!img.getAttribute('src')) return;
          img.complete = false;
          setTimeout(function () {
            img.complete = true;
            img.naturalWidth = img.naturalWidth || img.width || 0;
            img.naturalHeight = img.naturalHeight || img.height || 0;
            try { img.dispatchEvent(new Event('load')); } catch (e) {}
          }, 0);
        },
      });
    }
    return img;
  };
  globalThis.Image.prototype = globalThis.HTMLImageElement.prototype;
}

if (typeof Audio === 'undefined') {
  globalThis.Audio = class Audio {
    constructor(src) { this.src = src || ''; this.paused = true; this.volume = 1; this.currentTime = 0; this.duration = 0; }
    play() { return Promise.resolve(); } pause() { this.paused = true; } load() {}
    addEventListener() {} removeEventListener() {}
  };
}

if (typeof FileReader === 'undefined') {
  globalThis.FileReader = class FileReader {
    constructor() {
      this.result = null; this.error = null; this.readyState = 0; // EMPTY
      this.onloadstart = null; this.onprogress = null; this.onload = null;
      this.onabort = null; this.onerror = null; this.onloadend = null;
      this._listeners = {};
    }
    get [Symbol.toStringTag]() { return "FileReader"; }
    _read(blob, kind, encoding) {
      // Spec: reading while LOADING throws InvalidStateError.
      if (this.readyState === 1) throw new DOMException("The object is already busy reading Blobs.", "InvalidStateError");
      this.readyState = 1; // LOADING
      this.result = null; this.error = null;
      this._fire("loadstart");
      const self = this;
      Promise.resolve().then(function () {
        if (self.readyState !== 1) return; // aborted before completion
        const bytes = (blob && blob._bytes) ? blob._bytes : new Uint8Array(0);
        try {
          if (kind === "text") self.result = new TextDecoder(encoding || "utf-8").decode(bytes);
          else if (kind === "binary") self.result = _bytesToBinaryString(bytes);
          else if (kind === "dataurl") self.result = "data:" + ((blob && blob.type) || "application/octet-stream") + ";base64," + btoa(_bytesToBinaryString(bytes));
          else self.result = _arrayBufferFromBytes(bytes);
        } catch (e) { self.error = e; }
        self.readyState = 2; // DONE
        self._fire("progress"); self._fire("load"); self._fire("loadend");
      });
    }
    readAsText(blob, encoding) { this._read(blob, "text", encoding); }
    readAsDataURL(blob) { this._read(blob, "dataurl"); }
    readAsArrayBuffer(blob) { this._read(blob, "arraybuffer"); }
    readAsBinaryString(blob) { this._read(blob, "binary"); }
    abort() {
      const wasReading = this.readyState === 1;
      this.readyState = 0; this.result = null;
      if (wasReading) { this._fire("abort"); this._fire("loadend"); }
    }
    _fire(type) {
      const ev = { type: type, target: this, currentTarget: this, lengthComputable: false, loaded: 0, total: 0 };
      const h = this["on" + type]; if (typeof h === "function") { try { h.call(this, ev); } catch (e) {} }
      const ls = this._listeners[type]; if (ls) for (const fn of ls.slice()) { try { fn.call(this, ev); } catch (e) {} }
    }
    addEventListener(t, fn) { if (typeof fn === "function") (this._listeners[t] = this._listeners[t] || []).push(fn); }
    removeEventListener(t, fn) { const ls = this._listeners[t]; if (ls) { const i = ls.indexOf(fn); if (i >= 0) ls.splice(i, 1); } }
    dispatchEvent() { return true; }
  };
  globalThis.FileReader.EMPTY = 0; globalThis.FileReader.LOADING = 1; globalThis.FileReader.DONE = 2;
  Object.assign(globalThis.FileReader.prototype, { EMPTY: 0, LOADING: 1, DONE: 2 });
}

// Real network sockets aren't implemented; we don't have a runtime WS / SSE
// client in V8. But pages that wait for an `open` event (Vite HMR clients
// embedded on docs sites, live-dashboards, anything calling
// `await new Promise(r => ws.addEventListener('open', r))`) silently hang
// forever otherwise. Fire `open` after a microtask so the consumer at least
// proceeds; subsequent messages never arrive, which is no worse than the
// current "no signal whatsoever" behaviour.
// Minimal EventTarget shared by socket-like classes. Real `EventTarget` is
// currently aliased to `Node`, which would drag DOM-tree assumptions into a
// `WebSocket`. Defining a private shim avoids that.
function _makeListenerBox(self) {
  const map = new Map();
  self.addEventListener = function (type, fn) {
    if (typeof fn !== 'function') return;
    let bucket = map.get(type);
    if (!bucket) { bucket = []; map.set(type, bucket); }
    bucket.push(fn);
  };
  self.removeEventListener = function (type, fn) {
    const bucket = map.get(type);
    if (!bucket) return;
    const i = bucket.indexOf(fn);
    if (i >= 0) bucket.splice(i, 1);
  };
  self.dispatchEvent = function (event) {
    const bucket = map.get(event.type);
    if (!bucket) return true;
    for (const fn of bucket.slice()) {
      try { fn.call(self, event); } catch (e) { /* swallow */ }
    }
    return true;
  };
}

if (typeof EventSource === 'undefined') {
  globalThis.EventSource = class EventSource {
    constructor(url, init) {
      this.url = url;
      this.readyState = 0; // CONNECTING
      this.withCredentials = !!(init && init.withCredentials);
      this.onopen = null; this.onmessage = null; this.onerror = null;
      _makeListenerBox(this);
      Promise.resolve().then(() => {
        if (this.readyState !== 0) return;
        this.readyState = 1; // OPEN
        const ev = new Event('open');
        if (typeof this.onopen === 'function') { try { this.onopen(ev); } catch (e) {} }
        try { this.dispatchEvent(ev); } catch (e) {}
      });
    }
    close() { this.readyState = 2; }
    static CONNECTING = 0; static OPEN = 1; static CLOSED = 2;
  };
}

if (typeof WebSocket === 'undefined') {
  globalThis.WebSocket = class WebSocket {
    constructor(url, protocols) {
      this.url = url;
      this.readyState = 0; // CONNECTING
      this.bufferedAmount = 0;
      this.binaryType = 'blob';
      this.extensions = '';
      this.protocol = Array.isArray(protocols) ? (protocols[0] || '') : (protocols || '');
      this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null;
      _makeListenerBox(this);
      Promise.resolve().then(() => {
        if (this.readyState !== 0) return;
        this.readyState = 1; // OPEN
        const ev = new Event('open');
        if (typeof this.onopen === 'function') { try { this.onopen(ev); } catch (e) {} }
        try { this.dispatchEvent(ev); } catch (e) {}
      });
    }
    send(data) { /* drop; no real socket */ }
    close(code, reason) {
      if (this.readyState >= 2) return;
      this.readyState = 3; // CLOSED
      const ev = new Event('close');
      ev.code = code || 1000; ev.reason = reason || ''; ev.wasClean = true;
      if (typeof this.onclose === 'function') { try { this.onclose(ev); } catch (e) {} }
      try { this.dispatchEvent(ev); } catch (e) {}
    }
    static CONNECTING = 0; static OPEN = 1; static CLOSING = 2; static CLOSED = 3;
  };
}

if (typeof BroadcastChannel === 'undefined') {
  globalThis.BroadcastChannel = class BroadcastChannel {
    constructor(name) {
      this.name = name; this.onmessage = null; this.onmessageerror = null;
      _makeListenerBox(this);
    }
    postMessage(msg) {}
    close() {}
  };
}

if (typeof MediaQueryList === 'undefined') {
  globalThis.MediaQueryList = class MediaQueryList {
    constructor(q) { this.media = q || ''; this.matches = false; }
    addListener() {} removeListener() {} addEventListener() {} removeEventListener() {}
  };
}

if (typeof ImageData === 'undefined') {
  globalThis.ImageData = class ImageData {
    constructor(w, h) {
      if (w instanceof Uint8ClampedArray) { this.data = w; this.width = h; this.height = w.length / (4 * h); }
      else { this.width = w; this.height = h; this.data = new Uint8ClampedArray(w * h * 4); }
    }
  };
}

if (typeof CanvasRenderingContext2D === 'undefined') {
  globalThis.CanvasRenderingContext2D = class CanvasRenderingContext2D {};
}

if (typeof OffscreenCanvas === 'undefined') {
  globalThis.OffscreenCanvas = class OffscreenCanvas {
    constructor(w, h) { this.width = w; this.height = h; }
    getContext(type) { return globalThis.document?.createElement('canvas')?.getContext(type) || null; }
    convertToBlob() { return Promise.resolve(new Blob([''])); }
    transferToImageBitmap() { return {}; }
  };
}

if (typeof Path2D === 'undefined') {
  globalThis.Path2D = class Path2D { constructor(){} moveTo(){} lineTo(){} arc(){} rect(){} closePath(){} addPath(){} };
}

if (typeof ImageBitmap === 'undefined') {
  globalThis.ImageBitmap = class ImageBitmap { constructor(){this.width=0;this.height=0;} close(){} };
  globalThis.createImageBitmap = function() { return Promise.resolve(new ImageBitmap()); };
}

if (typeof Selection === 'undefined') {
  globalThis.Selection = class Selection {
    constructor(){this.anchorNode=null;this.focusNode=null;this.rangeCount=0;this.isCollapsed=true;this.type='None';}
    getRangeAt(){return null;} collapse(){} extend(){} selectAllChildren(){} deleteFromDocument(){}
    addRange(){} removeRange(){} removeAllRanges(){} toString(){return '';}
  };
}

if (typeof TreeWalker === 'undefined') {
  globalThis.TreeWalker = class TreeWalker {
    constructor(root){this.root=root;this.currentNode=root;this.whatToShow=0xFFFFFFFF;this.filter=null;}
    parentNode(){return this.currentNode?.parentNode||null;}
    firstChild(){return this.currentNode?.firstChild||null;}
    lastChild(){return this.currentNode?.lastChild||null;}
    previousSibling(){return this.currentNode?.previousSibling||null;}
    nextSibling(){return this.currentNode?.nextSibling||null;}
    nextNode(){return null;} previousNode(){return null;}
  };
}

if (typeof Range === 'undefined') {
  globalThis.Range = class Range {
    constructor(){this.startContainer=null;this.startOffset=0;this.endContainer=null;this.endOffset=0;this.collapsed=true;this.commonAncestorContainer=null;}
    setStart(n,o){this.startContainer=n;this.startOffset=o;} setEnd(n,o){this.endContainer=n;this.endOffset=o;}
    collapse(){} selectNode(){} selectNodeContents(){} cloneContents(){return document?.createDocumentFragment();}
    deleteContents(){} insertNode(){} getBoundingClientRect(){return new DOMRect();}
    getClientRects(){return [];} cloneRange(){return new Range();} toString(){return '';}
  };
}

if (typeof SharedWorker === 'undefined') {
  globalThis.SharedWorker = class SharedWorker {
    constructor() { this.port = { postMessage(){}, onmessage:null, start(){}, close(){}, addEventListener(){}, removeEventListener(){} }; this.onerror = null; }
  };
}
if (typeof ServiceWorkerContainer === 'undefined') {
  globalThis.ServiceWorkerContainer = class { register(){return Promise.resolve();} getRegistrations(){return Promise.resolve([]);} };
}

if (typeof URLPattern === 'undefined') {
  globalThis.URLPattern = class URLPattern {
    constructor(pattern){this._pattern=pattern||{};} test(){return false;} exec(){return null;}
  };
}

if (typeof Document !== 'undefined' && !Document.prototype.importNode) {
  Document.prototype.importNode = function(node, deep) { return node?.cloneNode(!!deep) || null; };
}
// Document.adoptNode: standard DOM (HTML living spec). Frameworks that move
// nodes between documents (portals, iframe hand-off) call it; the missing
// method throws "adoptNode is not a function". With no second document to
// transfer ownership from, the node is already ours, so return it as-is,
// matching the observable effect of adoption into this document.
if (typeof Document !== 'undefined' && !Document.prototype.adoptNode) {
  Document.prototype.adoptNode = function(node) { return node || null; };
  _markNative(Document.prototype.adoptNode);
}
// Element.toggleAttribute: standard DOM. Lit/Stencil and several ad SDKs call
// it; the missing method throws. Spec semantics: no force arg toggles, force
// true adds, force false removes; returns the new presence.
if (typeof Element !== 'undefined' && !Element.prototype.toggleAttribute) {
  Element.prototype.toggleAttribute = function(name, force) {
    const n = String(name);
    const present = this.hasAttribute(n);
    const want = arguments.length < 2 ? !present : !!force;
    if (want && !present) { this.setAttribute(n, ''); return true; }
    if (!want && present) { this.removeAttribute(n); return false; }
    return want;
  };
  _markNative(Element.prototype.toggleAttribute);
}

// Document.elementFromPoint / elementsFromPoint — hit testing against the
// element rects (real layout geometry under the screenshot build, the
// synthetic grid otherwise; see issue #63 for the original stub rationale).
// Flat iteration over every element, NOT a tree walk: rects don't always
// nest (a synthetic child rect can lie outside its parent's), so a walk
// that only descends into containing ancestors would miss deep elements.
//
// Which overlapping candidate wins is a PAINT question, not a document
// order question (obscura #738): a close button with z-index 1002 that
// precedes a z-index 1001 overlay in the DOM is the element the pixel
// shows. The Rust side exports the layout walk's paint order
// (_domRaw("paint_order")); candidates rank by it, ties (and the
// no-layout fallback) fall back to document order via nid. Boxless inline
// wrappers (span/a/label) never get their own slot — rank them with their
// nearest boxed ancestor, whose subtree painted their hoisted ink anyway.
if (typeof Document !== 'undefined' && !Document.prototype.elementFromPoint) {
  function __ditingPaintRanks() {
    try {
      var raw = _domRaw("paint_order", "", "");
      if (!raw) return null;
      var ids = JSON.parse(raw);
      if (!ids || !ids.length) return null;
      var rank = {};
      for (var i = 0; i < ids.length; i++) rank[ids[i]] = i;
      return rank;
    } catch (e) {
      return null;
    }
  }
  function __ditingPaintRankOf(el, rank) {
    var v = rank[el._nid | 0];
    if (v !== undefined) return v;
    for (var p = el.parentElement; p; p = p.parentElement) {
      var pv = rank[p._nid | 0];
      if (pv !== undefined) return pv;
    }
    return -1;
  }
  Document.prototype.elementFromPoint = function(x, y) {
    var cands = __ditingHitCandidates.call(this, x, y);
    if (cands === null) return null;
    return cands.length ? cands[0] : (this.body || this.documentElement || null);
  };
  Document.prototype.elementsFromPoint = function(x, y) {
    var cands = __ditingHitCandidates.call(this, x, y);
    if (cands === null) return [];
    if (cands.length) return cands;
    var el = this.elementFromPoint(x, y); // body fallback, as before
    return el ? [el] : [];
  };
  // Shared candidate walk: every non-root element whose rect contains
  // (x, y), sorted front-to-back by (paint rank, document order). Invalid
  // coordinates return null so both entry points can distinguish "miss"
  // from "empty list".
  function __ditingHitCandidates(x, y) {
    if (typeof x !== 'number' || typeof y !== 'number' || !isFinite(x) || !isFinite(y)) {
      return null;
    }
    var w = (typeof window !== 'undefined' && window.innerWidth) || 1280;
    var h = (typeof window !== 'undefined' && window.innerHeight) || 720;
    if (x < 0 || y < 0 || x > w || y > h) return null;
    var all = this.querySelectorAll('*');
    var rank = __ditingPaintRanks();
    var cands = [];
    for (var i = 0; i < all.length; i++) {
      var el = all[i];
      if (!el || !el.getBoundingClientRect) continue;
      // documentElement / body span the viewport; skip them so we pick a
      // real descendant instead of falling back to <html>/<body>.
      if (el === this.documentElement || el === this.body) continue;
      var r = el.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) continue;
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) {
        cands.push(el);
      }
    }
    if (!cands.length) return cands;
    cands.sort(function(a, b) {
      // No paint table (default build / synthetic grid): document order,
      // deepest (highest nid) wins — the pre-#738 behavior.
      if (!rank) return (b._nid | 0) - (a._nid | 0);
      return __ditingPaintRankOf(b, rank) - __ditingPaintRankOf(a, rank)
        || (b._nid | 0) - (a._nid | 0);
    });
    return cands;
  }
}
if (typeof ShadowRoot !== 'undefined' && !ShadowRoot.prototype.elementFromPoint) {
  ShadowRoot.prototype.elementFromPoint = function(x, y) {
    return Document.prototype.elementFromPoint.call(globalThis.document || this, x, y);
  };
  ShadowRoot.prototype.elementsFromPoint = function(x, y) {
    return Document.prototype.elementsFromPoint.call(globalThis.document || this, x, y);
  };
}

// (Re)apply the platform persona: screen, dpr, hardwareConcurrency,
// deviceMemory. Called from __diting_init AND again from Rust's
// set_user_agent — the runtime constructor runs init before the context's
// UA is known, so the persona must refresh once the real UA lands.
globalThis.__diting_setPersona = function() {
  const scr = _fp('screen');
  const sw = scr[0], sh = scr[1];
  globalThis.screen = { width:sw, height:sh, availWidth:sw, availHeight:sh-40, colorDepth:24, pixelDepth:24, availTop:0, availLeft:0, orientation:{type:"landscape-primary",angle:0,addEventListener(){},removeEventListener(){},dispatchEvent(){return true;}} };
  globalThis.visualViewport = { width:sw, height:sh-80, offsetLeft:0, offsetTop:0, scale:1, addEventListener(){}, removeEventListener(){} };
  // From the persona pool, so a retina Mac panel reports 2x (the old
  // width-only heuristic gave 1x on 1512x982 — impossible for that panel).
  globalThis.devicePixelRatio = _fp('dpr') || (sw >= 2560 ? 2 : 1);
  globalThis.innerWidth = sw; globalThis.innerHeight = sh - 80;
  globalThis.outerWidth = sw; globalThis.outerHeight = sh;
  // Publish the persona viewport to the Rust layout layer so
  // getBoundingClientRect / element rects anchor the initial containing
  // block to the same window scripts see (not the pre-persona default).
  try { _domRaw("set_viewport", String(sw), String(sh - 80)); } catch (e) {}

  // Stable for the life of the process (drawn once), not per navigation —
  // hardwareConcurrency flipping between pages of one visit is its own
  // automation tell. Values are platform-plausible pairs: Chrome caps
  // deviceMemory at 8, and 16-thread machines never report 2 GB.
  const plat = _fpPlatform();
  if (globalThis.__diting_hw === undefined || globalThis.__diting_hw_plat !== plat) {
    let hws, mems;
    if (plat === 'mac') { hws = [8, 10, 12]; mems = [8]; }
    else if (plat === 'win') { hws = [4, 6, 8, 12, 16, 20, 24]; mems = [4, 8]; }
    else { hws = [4, 6, 8, 12, 16]; mems = [4, 8]; }
    globalThis.__diting_hw = hws[Math.floor(_fpRand(400) * hws.length)];
    globalThis.__diting_mem = mems[Math.floor(_fpRand(401) * mems.length)];
    globalThis.__diting_hw_plat = plat;
  }
  globalThis.navigator.hardwareConcurrency = globalThis.__diting_hw;
  globalThis.navigator.deviceMemory = globalThis.__diting_mem;
};

globalThis.__diting_init = function() {
  _fpSeed = Date.now() ^ (Math.random() * 0xFFFFFFFF >>> 0);
  _fpCache = null;
  // A real navigation just completed (this runs after set_url), so drop any
  // URL a location setter previewed synchronously and let document_url drive
  // location.href again, including any redirect target.
  globalThis.__virtualUrl = null;
  _installWasmStreamingFallback();

  globalThis.__diting_setPersona();

  const t0 = Date.now() + Math.floor(_fpRand(641) * 100) - 50;
  // NavigationTiming offsets: DCL 60–600ms after navigationStart, load
  // DCL+40–800ms after that, with a plausible connect/response chain ahead
  // of domInteractive. All-equal epoch stamps (the old shape) made every
  // derived duration 0 — telemetry reading getEntriesByType('navigation')
  // got a zero-duration page, which is its own automation tell.
  const dcl = 60 + Math.floor(_fpRand(642) * 540);
  const load = dcl + 40 + Math.floor(_fpRand(643) * 760);
  globalThis.performance.timeOrigin = t0;
  globalThis.performance.timing = {
    navigationStart: t0,
    fetchStart: t0 + 1, domainLookupStart: t0 + 2, domainLookupEnd: t0 + 12,
    connectStart: t0 + 12, connectEnd: t0 + 48, secureConnectionStart: t0 + 18,
    requestStart: t0 + 50, responseStart: t0 + 85, responseEnd: t0 + 130,
    domInteractive: t0 + Math.floor(dcl * 0.8),
    domContentLoadedEventStart: t0 + dcl - 4, domContentLoadedEventEnd: t0 + dcl,
    domComplete: t0 + load, loadEventStart: t0 + load, loadEventEnd: t0 + load,
  };
  // Derived entries (navigation/paint) are cached per navigation.
  if (globalThis.performance._resetDerived) globalThis.performance._resetDerived();
  globalThis.performance.memory = {
    jsHeapSizeLimit: 2172649472,
    totalJSHeapSize: 15000000 + Math.floor(_fpRand(620) * 85000000),
    usedJSHeapSize: 8000000 + Math.floor(_fpRand(621) * 42000000),
  };
  globalThis.Notification.permission = _fpRand(630) > 0.5 ? "granted" : "default";

  // Hide internals (_*, obscura, Obscura). The set of keys is static at
  // snapshot-build time, so we precompute it ONCE below (after this
  // function definition) and reuse it on every page init. Was an
  // Object.keys + filter on every navigation, ~5-40ms per page on
  // SPAs that load 1000+ globals.
  const toHide = globalThis.__diting_hide_list || [];
  for (let i = 0; i < toHide.length; i++) {
    try { Object.defineProperty(globalThis, toHide[i], { enumerable: false }); } catch(e) {}
  }

  // Strip the deno runtime globals from the page's window. A `Deno` (or
  // `__bootstrap`) own property on window is the canonical marker of a
  // deno-based runtime — precisely what an AI-agent-blocker looks for — and
  // it survives enumerable:false because collectors enumerate with
  // getOwnPropertyNames, not Object.keys. All engine op calls run through
  // the script-scoped `_OPS` binding, so the globals are dead weight here.
  // Both are configurable in deno_core and delete cleanly; keep the try
  // guards in case a future deno_core makes them non-configurable.
  try { delete globalThis.Deno; } catch(e) {}
  try { delete globalThis.__bootstrap; } catch(e) {}

  // Non-configurable function declarations above (the engine's `_`-prefixed
  // helpers and `__diting*` bookkeeping) cannot be deleted, so hide them at
  // the enumeration boundary instead: Radar's windowFeatures collector
  // hashes Object.getOwnPropertyNames(window), and fingerprinting scripts
  // also enumerate with Reflect.ownKeys / Object.keys /
  // Object.getOwnPropertyDescriptors (upstream 4c33f6d). The wrappers only
  // filter the global object — every other receiver sees the untouched
  // native result — and are marked native so toString lie-detectors report
  // [native code].
  if (!globalThis.__diting_gopn_patched) {
    // Pattern, not the static hide list: `__diting_objects` & friends are
    // created by the Rust init AFTER the snapshot froze the list, so they'd
    // slip through a membership check against it.
    const _isInternal = n => typeof n === 'string' && (n.startsWith('_') || n.includes('obscura') || n.includes('Obscura') || n === '__bootstrap');
    const _gopn = Object.getOwnPropertyNames;
    const _ownKeys = Reflect.ownKeys;
    const _keys = Object.keys;
    const _gopds = Object.getOwnPropertyDescriptors;
    const _patch = (obj, key, impl) => {
      _markNative(impl);
      try { Object.defineProperty(impl, 'name', { value: key }); } catch(e) {}
      try { Object.defineProperty(obj, key, { value: impl, writable: true, enumerable: false, configurable: true }); } catch(e) {}
    };
    _patch(Object, 'getOwnPropertyNames', function(o) {
      const names = _gopn(o);
      if (o !== globalThis) return names;
      return names.filter(n => !_isInternal(n));
    });
    _patch(Reflect, 'ownKeys', function(o) {
      const names = _ownKeys(o);
      if (o !== globalThis) return names;
      return names.filter(n => !_isInternal(n));
    });
    _patch(Object, 'keys', function(o) {
      const names = _keys(o);
      if (o !== globalThis) return names;
      return names.filter(n => !_isInternal(n));
    });
    _patch(Object, 'getOwnPropertyDescriptors', function(o) {
      const all = _gopds(o);
      if (o !== globalThis) return all;
      for (const n of _gopn(all)) { if (_isInternal(n)) delete all[n]; }
      return all;
    });
    globalThis.__diting_gopn_patched = true;
  }
  delete globalThis.__diting_init;
};

// Snapshot-time pre-computation of the hide list. Bootstrap.js runs once
// during the V8 snapshot build (build.rs); this line captures the set of
// globals defined by bootstrap that we want to hide and stashes them
// for __diting_init to consume on every subsequent page. The snapshot
// preserves the array as a regular global.
// getOwnPropertyNames, not Object.keys: internals already made non-enumerable
// before this line would be omitted by Object.keys and escape the per-page
// hiding below (upstream 4c33f6d).
globalThis.__diting_hide_list = Object.getOwnPropertyNames(globalThis).filter(k =>
  k.startsWith('_') || k.includes('obscura') || k.includes('Obscura')
);

/* ===== WPT conformance shims: batch 2 ===== */

// ---- Node namespace lookup methods ----

Node.prototype.lookupNamespaceURI = function(prefix) {
  let node = this;
  if (node.nodeType === 9) node = node.documentElement;
  if (!node || node.nodeType !== 1) return null;
  const _ns_builtins = { 'xml': 'http://www.w3.org/XML/1998/namespace', 'xmlns': 'http://www.w3.org/2000/xmlns/' };
  if (prefix && _ns_builtins[prefix]) return _ns_builtins[prefix];
  while (node && node.nodeType === 1) {
    if (prefix) {
      if (node.prefix === prefix && node.namespaceURI) return node.namespaceURI;
      const nsAttr = node.getAttribute('xmlns:' + prefix);
      if (nsAttr !== null) return nsAttr || null;
    } else {
      const defaultNs = node.getAttribute('xmlns');
      if (defaultNs !== null) return defaultNs || null;
      if (node.prefix === null && node.namespaceURI) return node.namespaceURI;
    }
    node = node.parentElement;
  }
  return null;
};
_markNative(Node.prototype.lookupNamespaceURI);

Node.prototype.lookupPrefix = function(namespace) {
  namespace = namespace || null;
  let node = this;
  if (node.nodeType === 9) node = node.documentElement;
  if (!node || node.nodeType !== 1) return null;
  const _ns_builtins = { 'http://www.w3.org/XML/1998/namespace': 'xml', 'http://www.w3.org/2000/xmlns/': 'xmlns' };
  if (_ns_builtins[namespace]) return _ns_builtins[namespace];
  while (node && node.nodeType === 1) {
    if (node.namespaceURI === namespace) {
      const p = node.prefix;
      if (p) return p;
    }
    const attrs = node.attributes || [];
    for (let i = 0; i < attrs.length; i++) {
      const attr = attrs[i];
      const attrName = attr.name || attr.nodeName || '';
      const attrValue = attr.value || attr.nodeValue || '';
      if (attrName === 'xmlns' && attrValue === namespace) return '';
      if (attrName.startsWith('xmlns:')) {
        const prefix = attrName.substring(6);
        if (attrValue === namespace) return prefix;
      }
    }
    node = node.parentElement;
  }
  return null;
};
_markNative(Node.prototype.lookupPrefix);

Node.prototype.isDefaultNamespace = function(namespace) {
  return this.lookupNamespaceURI(null) === (namespace || null);
};
_markNative(Node.prototype.isDefaultNamespace);


// ---- getElementsByTagNameNS on Element and Document ----
// getElementsByTagNameNS on Element and Document
if (!Element.prototype.getElementsByTagNameNS) {
  Element.prototype.getElementsByTagNameNS = function(namespaceURI, localName) {
    const all = this.querySelectorAll('*');
    const filtered = [];
    const nsMatch = namespaceURI === '*';
    const tagMatch = localName === '*';
    for (let i = 0; i < all.length; i++) {
      const el = all[i];
      if (!el) continue;
      const elNs = el.namespaceURI;
      const elTag = el.localName;
      const nsOk = nsMatch || (elNs === (namespaceURI || null));
      const tagOk = tagMatch || (elTag === localName);
      if (nsOk && tagOk) filtered.push(el);
    }
    const result = new HTMLCollection(...filtered);
    result.item = (i) => result[i] != null ? result[i] : null;
    return result;
  };
  _markNative(Element.prototype.getElementsByTagNameNS);
}
if (!Document.prototype.getElementsByTagNameNS) {
  Document.prototype.getElementsByTagNameNS = function(namespaceURI, localName) {
    const all = this.querySelectorAll('*');
    const filtered = [];
    const nsMatch = namespaceURI === '*';
    const tagMatch = localName === '*';
    for (let i = 0; i < all.length; i++) {
      const el = all[i];
      if (!el) continue;
      const elNs = el.namespaceURI;
      const elTag = el.localName;
      const nsOk = nsMatch || (elNs === (namespaceURI || null));
      const tagOk = tagMatch || (elTag === localName);
      if (nsOk && tagOk) filtered.push(el);
    }
    const result = new HTMLCollection(...filtered);
    result.item = (i) => result[i] != null ? result[i] : null;
    return result;
  };
  _markNative(Document.prototype.getElementsByTagNameNS);
}

// ---- Attr nodes and createAttribute ----
// Attr class: represents attribute nodes (nodeType 2)
if (!globalThis.Attr) {
  globalThis.Attr = class Attr {
    constructor(name, value = '', namespaceURI = null, prefix = null) {
      this.name = name;
      this.localName = name;
      this.value = value;
      this.namespaceURI = namespaceURI;
      this.prefix = prefix;
      this.ownerElement = null;
      this.specified = true;
    }
    get nodeName() { return this.name; }
    get nodeValue() { return this.value; }
    set nodeValue(v) { this.value = v; }
    get nodeType() { return 2; }
  };
}

// XML Name validation helper for attribute/processing instruction names
const _ns_isValidXmlName = (name) => {
  if (typeof name !== 'string' || !name.length) return false;
  return /^[A-Za-z_:][\w.\-:]*$/.test(name);
};

// Document.prototype.createAttribute: create a detached Attr node
if (!Document.prototype.createAttribute) {
  Document.prototype.createAttribute = function(localName) {
    const name = String(localName || '');
    if (!_ns_isValidXmlName(name)) {
      throw new DOMException('Invalid attribute name', 'InvalidCharacterError');
    }
    return new Attr(name, '', null, null);
  };
  _markNative(Document.prototype.createAttribute);
}

// Document.prototype.createAttributeNS: create a namespaced Attr node
if (!Document.prototype.createAttributeNS) {
  Document.prototype.createAttributeNS = function(namespaceURI, qualifiedName) {
    const ns = namespaceURI ? String(namespaceURI) : null;
    const qn = String(qualifiedName || '');
    if (!qn.length) {
      throw new DOMException('Invalid attribute name', 'InvalidCharacterError');
    }
    let prefix = null;
    let localName = qn;
    const colonIdx = qn.indexOf(':');
    if (colonIdx !== -1) {
      prefix = qn.substring(0, colonIdx);
      localName = qn.substring(colonIdx + 1);
      if (!_ns_isValidXmlName(prefix) || !_ns_isValidXmlName(localName)) {
        throw new DOMException('Invalid attribute name', 'InvalidCharacterError');
      }
    } else {
      if (!_ns_isValidXmlName(localName)) {
        throw new DOMException('Invalid attribute name', 'InvalidCharacterError');
      }
    }
    return new Attr(qn, '', ns, prefix);
  };
  _markNative(Document.prototype.createAttributeNS);
}

// Element.prototype.getAttributeNode: return an Attr node or null
if (!Element.prototype.getAttributeNode) {
  Element.prototype.getAttributeNode = function(name) {
    const val = this.getAttribute(name);
    if (val === null) return null;
    const attr = new Attr(name, val, null, null);
    attr.ownerElement = this;
    return attr;
  };
  _markNative(Element.prototype.getAttributeNode);
}

// Element.prototype.getAttributeNodeNS: return a namespaced Attr node or null
if (!Element.prototype.getAttributeNodeNS) {
  Element.prototype.getAttributeNodeNS = function(namespaceURI, localName) {
    const val = this.getAttributeNS(namespaceURI, localName);
    if (val === null) return null;
    const name = String(localName || '');
    const attr = new Attr(name, val, namespaceURI ? String(namespaceURI) : null, null);
    attr.ownerElement = this;
    return attr;
  };
  _markNative(Element.prototype.getAttributeNodeNS);
}

// Element.prototype.setAttributeNode: set an Attr and return the previous one
if (!Element.prototype.setAttributeNode) {
  Element.prototype.setAttributeNode = function(attr) {
    if (!attr || typeof attr.name !== 'string') return null;
    const prevVal = this.getAttribute(attr.name);
    const prevAttr = prevVal !== null ? new Attr(attr.name, prevVal, null, null) : null;
    if (prevAttr) prevAttr.ownerElement = this;
    this.setAttribute(attr.name, attr.value);
    attr.ownerElement = this;
    return prevAttr;
  };
  _markNative(Element.prototype.setAttributeNode);
}

// Element.prototype.setAttributeNodeNS: set a namespaced Attr and return the previous one
if (!Element.prototype.setAttributeNodeNS) {
  Element.prototype.setAttributeNodeNS = function(attr) {
    if (!attr || typeof attr.name !== 'string') return null;
    const prevVal = this.getAttribute(attr.name);
    const prevAttr = prevVal !== null 
      ? new Attr(attr.name, prevVal, attr.namespaceURI || null, attr.prefix || null) 
      : null;
    if (prevAttr) prevAttr.ownerElement = this;
    this.setAttributeNS(attr.namespaceURI || null, attr.name, attr.value);
    attr.ownerElement = this;
    return prevAttr;
  };
  _markNative(Element.prototype.setAttributeNodeNS);
}

// Element.prototype.removeAttributeNode: remove and return an Attr
if (!Element.prototype.removeAttributeNode) {
  Element.prototype.removeAttributeNode = function(attr) {
    if (!attr || typeof attr.name !== 'string') return attr;
    const val = this.getAttribute(attr.name);
    if (val !== null) {
      this.removeAttribute(attr.name);
    }
    return attr;
  };
  _markNative(Element.prototype.removeAttributeNode);
}


// ---- form control validity and text selection ----

// ValidityState class for form validation state reporting
if (typeof ValidityState === 'undefined') {
  globalThis.ValidityState = class ValidityState {
    constructor() {
      this.badInput = false;
      this.customError = false;
      this.patternMismatch = false;
      this.rangeOverflow = false;
      this.rangeUnderflow = false;
      this.stepMismatch = false;
      this.tooLong = false;
      this.tooShort = false;
      this.typeMismatch = false;
      this.valueMissing = false;
      this.valid = true;
    }
  };
}

// Validity and validation message storage on elements
const _ns_validityCache = new WeakMap();
const _ns_customValidityMsg = new WeakMap();

// Element.prototype.validity - returns cached ValidityState for the element
if (!Element.prototype.validity) {
  Object.defineProperty(Element.prototype, 'validity', {
    get: function() {
      if (!_ns_validityCache.has(this)) {
        _ns_validityCache.set(this, new ValidityState());
      }
      return _ns_validityCache.get(this);
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.willValidate - whether element is subject to constraint validation
if (!Element.prototype.willValidate) {
  Object.defineProperty(Element.prototype, 'willValidate', {
    get: function() {
      return true;
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.validationMessage - custom validation message if set
if (!Element.prototype.validationMessage) {
  Object.defineProperty(Element.prototype, 'validationMessage', {
    get: function() {
      return _ns_customValidityMsg.get(this) || '';
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.checkValidity - stub returns true
if (!Element.prototype.checkValidity) {
  Element.prototype.checkValidity = function checkValidity() {
    return true;
  };
  _markNative(Element.prototype.checkValidity);
}

// Element.prototype.reportValidity - stub returns true
if (!Element.prototype.reportValidity) {
  Element.prototype.reportValidity = function reportValidity() {
    return true;
  };
  _markNative(Element.prototype.reportValidity);
}

// Element.prototype.setCustomValidity - set custom validation message
if (!Element.prototype.setCustomValidity) {
  Element.prototype.setCustomValidity = function setCustomValidity(msg) {
    const validity = this.validity;
    if (msg && msg.length > 0) {
      _ns_customValidityMsg.set(this, msg);
      validity.customError = true;
      validity.valid = false;
    } else {
      _ns_customValidityMsg.delete(this);
      validity.customError = false;
      validity.valid = true;
    }
  };
  _markNative(Element.prototype.setCustomValidity);
}

// Text selection on Element.prototype
const _ns_selectionStart = new WeakMap();
const _ns_selectionEnd = new WeakMap();
const _ns_selectionDir = new WeakMap();

// Element.prototype.selectionStart - get/set selection start position
if (!Element.prototype.selectionStart) {
  Object.defineProperty(Element.prototype, 'selectionStart', {
    get: function() {
      return _ns_selectionStart.get(this) ?? null;
    },
    set: function(v) {
      _ns_selectionStart.set(this, v == null ? null : Math.max(0, parseInt(v, 10) || 0));
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.selectionEnd - get/set selection end position
if (!Element.prototype.selectionEnd) {
  Object.defineProperty(Element.prototype, 'selectionEnd', {
    get: function() {
      return _ns_selectionEnd.get(this) ?? null;
    },
    set: function(v) {
      _ns_selectionEnd.set(this, v == null ? null : Math.max(0, parseInt(v, 10) || 0));
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.selectionDirection - get/set selection direction
if (!Element.prototype.selectionDirection) {
  Object.defineProperty(Element.prototype, 'selectionDirection', {
    get: function() {
      return _ns_selectionDir.get(this) ?? 'none';
    },
    set: function(v) {
      _ns_selectionDir.set(this, v === 'forward' || v === 'backward' ? v : 'none');
    },
    enumerable: true,
    configurable: true
  });
}

// Element.prototype.setSelectionRange - set text selection range
if (!Element.prototype.setSelectionRange) {
  Element.prototype.setSelectionRange = function setSelectionRange(start, end, direction) {
    start = Math.max(0, parseInt(start, 10) || 0);
    end = Math.max(0, parseInt(end, 10) || 0);
    direction = direction === 'forward' || direction === 'backward' ? direction : 'none';
    _ns_selectionStart.set(this, start);
    _ns_selectionEnd.set(this, end);
    _ns_selectionDir.set(this, direction);
  };
  _markNative(Element.prototype.setSelectionRange);
}

// Element.prototype.setRangeText - replace selection with text
if (!Element.prototype.setRangeText) {
  Element.prototype.setRangeText = function setRangeText(replacement, start, end, selectMode) {
    const val = this.value;
    if (!val) return;
    const strVal = String(val);
    start = start === undefined ? (this.selectionStart ?? 0) : Math.max(0, parseInt(start, 10) || 0);
    end = end === undefined ? (this.selectionEnd ?? 0) : Math.max(0, parseInt(end, 10) || 0);
    const newValue = strVal.slice(0, start) + String(replacement) + strVal.slice(end);
    this.value = newValue;
    selectMode = selectMode || 'preserve';
    if (selectMode === 'select') {
      const replLen = String(replacement).length;
      _ns_selectionStart.set(this, start);
      _ns_selectionEnd.set(this, start + replLen);
      _ns_selectionDir.set(this, 'none');
    } else if (selectMode === 'start') {
      _ns_selectionStart.set(this, start);
      _ns_selectionEnd.set(this, start);
      _ns_selectionDir.set(this, 'none');
    } else if (selectMode === 'end') {
      const replLen = String(replacement).length;
      _ns_selectionStart.set(this, start + replLen);
      _ns_selectionEnd.set(this, start + replLen);
      _ns_selectionDir.set(this, 'none');
    }
  };
  _markNative(Element.prototype.setRangeText);
}

// Element.prototype.select - select all text in the element
if (!Element.prototype.select) {
  Element.prototype.select = function select() {
    const val = this.value;
    if (val === undefined || val === null) return;
    const len = String(val).length;
    _ns_selectionStart.set(this, 0);
    _ns_selectionEnd.set(this, len);
    _ns_selectionDir.set(this, 'none');
  };
  _markNative(Element.prototype.select);
}


// ---- Response.blob() on the real fetch path ----

if (typeof Response !== 'undefined' && Response.prototype && !Response.prototype.blob) {
  Response.prototype.blob = async function() {
    const bytes = await this.arrayBuffer();
    const contentType = this.headers && typeof this.headers.get === 'function' ? this.headers.get('content-type') : '';
    return new Blob([new Uint8Array(bytes)], { type: contentType || '' });
  };
  _markNative(Response.prototype.blob);
}
if (typeof Response !== 'undefined' && Response.prototype && !Response.prototype.text) {
  Response.prototype.text = async function() {
    const buffer = await this.arrayBuffer();
    return new TextDecoder().decode(new Uint8Array(buffer));
  };
  _markNative(Response.prototype.text);
}
if (typeof Response !== 'undefined' && Response.prototype && !Response.prototype.json) {
  Response.prototype.json = async function() {
    return JSON.parse(await this.text());
  };
  _markNative(Response.prototype.json);
}
// arrayBuffer is the body primitive that blob/text/json derive from; the
// engine's Response provides it natively, so it is intentionally not shimmed
// here (a JS fallback could only recurse into itself).

// ---- GlobalEventHandlers `on*` IDL properties ----
// Real browsers expose every `on<event>` as an accessor property on
// Element.prototype, Document.prototype and window. Libraries feature-detect
// them: React's ChangeEventPlugin does `'oninput' in document` and, when that
// misses, falls back to an IE9-era propertychange path that silently swallows
// every synthetic `input` event - which is why filling React-controlled
// inputs (native value setter + dispatchEvent('input')) never reached
// onChange in this engine while `click` worked fine. The getter also
// compiles inline content attributes (`<div oninput="...">`), so
// `typeof el.oninput === 'function'` after setAttribute, like Chrome.
// (Fingerprint note: accessors live on the prototype, so they do not show up
// in Object.keys(instance) / hasOwnProperty scans of elements.)
const _GLOBAL_EVENT_NAMES = [
  'abort', 'animationcancel', 'animationend', 'animationiteration',
  'animationstart', 'auxclick', 'beforeinput', 'beforematch', 'beforetoggle',
  'blur', 'cancel', 'canplay', 'canplaythrough', 'change', 'click', 'close',
  'contextlost', 'contextmenu', 'contextrestored', 'copy', 'cuechange', 'cut',
  'dblclick', 'drag', 'dragend', 'dragenter', 'dragexit', 'dragleave',
  'dragover', 'dragstart', 'drop', 'durationchange', 'emptied', 'ended',
  'error', 'focus', 'focusin', 'focusout', 'formdata', 'gotpointercapture',
  'input', 'invalid', 'keydown', 'keypress', 'keyup', 'load', 'loadeddata',
  'loadedmetadata', 'loadstart', 'lostpointercapture', 'mousedown',
  'mouseenter', 'mouseleave', 'mousemove', 'mouseout', 'mouseover', 'mouseup',
  'paste', 'pause', 'play', 'playing', 'pointercancel', 'pointerdown',
  'pointerenter', 'pointerleave', 'pointermove', 'pointerout', 'pointerover',
  'pointerrawupdate', 'pointerup', 'progress', 'ratechange', 'reset',
  'resize', 'scroll', 'scrollend', 'securitypolicyviolation', 'seeked',
  'seeking', 'select', 'selectionchange', 'slotchange', 'stalled', 'submit',
  'suspend', 'timeupdate', 'toggle', 'touchcancel', 'touchend',
  'touchmove', 'touchstart', 'transitioncancel', 'transitionend',
  'transitionrun', 'transitionstart', 'volumechange', 'waiting', 'wheel',
];
function _defineOnHandler(obj, name, storeKey, inlineFallback) {
  if (Object.prototype.hasOwnProperty.call(obj, name)) return;
  Object.defineProperty(obj, name, {
    configurable: true,
    get() {
      const store = this[storeKey];
      if (store && Object.prototype.hasOwnProperty.call(store, name)) return store[name];
      // No JS handler assigned: reflect the inline content attribute, if any.
      if (inlineFallback && typeof this._resolveInlineHandler === 'function') {
        return this._resolveInlineHandler(name);
      }
      return null;
    },
    set(fn) {
      if (typeof fn !== 'function') fn = null;
      const store = this[storeKey] || (this[storeKey] = {});
      store[name] = fn;
    },
  });
}
for (const _n of _GLOBAL_EVENT_NAMES) _defineOnHandler(Element.prototype, 'on' + _n, '__onHandlers', true);
for (const _n of _GLOBAL_EVENT_NAMES) _defineOnHandler(Document.prototype, 'on' + _n, '__onHandlers', false);
// window-only handlers (WindowEventHandlers + the rest of GlobalEventHandlers).
// globalThis.onerror / onunhandledrejection are already real data properties;
// the `hasOwnProperty` guard above skips them, keeping the error capture intact.
const _WINDOW_EVENT_NAMES = _GLOBAL_EVENT_NAMES.concat([
  'afterprint', 'beforeprint', 'beforeunload', 'hashchange', 'languagechange',
  'message', 'messageerror', 'offline', 'online', 'pagehide', 'pageshow',
  'popstate', 'rejectionhandled', 'storage', 'unload',
]);
for (const _n of _WINDOW_EVENT_NAMES) _defineOnHandler(globalThis, 'on' + _n, '__windowOnHandlers', false);
// Fire the on* handler alongside addEventListener listeners for the two
// dispatch paths that did not consult it before (document and window).
// window.onerror is deliberately NOT consulted here: it is the uncaught-error
// logger, not a dispatchEvent target in this engine.
const _documentDispatch = Document.prototype.dispatchEvent;
Document.prototype.dispatchEvent = function(event) {
  if (event) {
    const handler = this['on' + event.type];
    if (typeof handler === 'function') {
      try {
        if (handler.call(this, event) === false && event.preventDefault) event.preventDefault();
      } catch (e) { console.error('document event error:', e); }
    }
  }
  return _documentDispatch.call(this, event);
};
_markNative(Document.prototype.dispatchEvent);
const _windowDispatch = globalThis.dispatchEvent;
globalThis.dispatchEvent = function(event) {
  if (event) {
    const store = globalThis.__windowOnHandlers || {};
    const key = 'on' + event.type;
    const handler = Object.prototype.hasOwnProperty.call(store, key) ? store[key] : null;
    if (typeof handler === 'function') {
      try {
        if (handler.call(globalThis, event) === false && event.preventDefault) event.preventDefault();
      } catch (e) { console.error(e); }
    }
  }
  return _windowDispatch.call(this, event);
};

// tamperedFunctions: every builtin constructor reachable from the global
// object gets its prototype methods AND accessors marked native, plus the
// constructor itself (upstream 4c33f6d). The per-site _markNative calls above
// miss accessors and several constructors; pixelscan's tamperedFunctions check
// flags e.g. an Element.prototype.nodeType getter whose toString leaks JS
// source. Runs once at snapshot build time; genuinely-native V8 builtins
// already report native, so only JS-backed members change.
(function _markBuiltinsNative() {
  const seen = new Set();
  function walk(ctor) {
    if (typeof ctor !== 'function') return;
    _markNative(ctor);
    const proto = ctor.prototype;
    if (!proto || seen.has(proto)) return;
    seen.add(proto);
    _markNativeProto(proto);
  }
  const names = Object.getOwnPropertyNames(globalThis);
  for (let i = 0; i < names.length; i++) {
    if (!/^[A-Z]/.test(names[i])) continue;
    let val;
    try { val = globalThis[names[i]]; } catch (e) { continue; }
    walk(val);
  }
})();

// WebIDL interface globals are non-enumerable in a real browser;
// `globalThis.X = X` assignments default to enumerable:true, and one line
// detects it: Object.getOwnPropertyDescriptor(window, 'Node').enumerable
// (upstream c7e7c70). In Chrome every capitalized global (all interfaces and
// JS builtins) is non-enumerable, so sweep by name shape. Runs at snapshot
// build time, before any page code; configurable is preserved so `var Node`
// pages still run.
(function _interfaceGlobalsNonEnumerable() {
  const names = Object.getOwnPropertyNames(globalThis);
  for (let i = 0; i < names.length; i++) {
    if (!/^[A-Z]/.test(names[i])) continue;
    let d;
    try { d = Object.getOwnPropertyDescriptor(globalThis, names[i]); } catch (e) { continue; }
    if (!d || !d.configurable || d.enumerable === false) continue;
    d.enumerable = false;
    try { Object.defineProperty(globalThis, names[i], d); } catch (e) {}
  }
})();
