// (#236 absorption split) The snapshot-time interface sweeps, moved out of
// bootstrap.js (god-file ratchet: the file only shrinks). These walk
// getOwnPropertyNames(globalThis) to finish EVERY interface — native-mark,
// non-enumerable globals, enumerable prototype members, WebIDL toStringTag
// brands — so they must run LAST, after bootstrap.js, xhr.js, message_port.js
// and fetch_probe.js have all installed their globals. Same realm, same
// snapshot: execute_script calls concatenate.

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

// (#27) Web IDL installs interface operations as {writable, enumerable,
// configurable} — but every method above was defined with plain assignment
// inside a class body (non-enumerable) or defineProperty without enumerable
// (defaults false). zone.js's patchClass() — the standard Angular/Protractor
// bootstrap — discovers methods via `for (const prop in instance)` and only
// walks ENUMERABLE properties, so it saw nothing but engine internal fields
// (_callback) and Angular died with "n.observe is not a function".
// Explicit-list sweep over the DOM-ish prototypes: never walk chains upward,
// that would reach Object.prototype and make hasOwnProperty etc. enumerable.
(function _makeInterfaceMembersEnumerable() {
  const protos = new Set();
  const add = (C) => {
    if (typeof C === "function" && C.prototype) protos.add(C.prototype);
  };
  for (const C of [
    globalThis.EventTarget, globalThis.Node, globalThis.Element,
    globalThis.HTMLElement, globalThis.HTMLFormElement, globalThis.Document,
    globalThis.DocumentFragment, globalThis.ShadowRoot, globalThis.Text,
    globalThis.Comment, globalThis.Attr, globalThis.CDATASection,
    globalThis.MutationObserver, globalThis.IntersectionObserver,
    globalThis.ResizeObserver, globalThis.PerformanceObserver,
    globalThis.FileReader, globalThis.XMLHttpRequest,
    globalThis.XMLHttpRequestEventTarget, globalThis.Image,
    globalThis.NodeList, globalThis.HTMLCollection, globalThis.DOMTokenList,
    globalThis.CSSStyleDeclaration, globalThis.Range, globalThis.Selection,
    globalThis.Option, globalThis.FormData, globalThis.Headers, globalThis.URL,
    globalThis.WebSocket, globalThis.EventSource, globalThis.BroadcastChannel,
    globalThis.ReadableStream, globalThis.WritableStream,
    globalThis.FileList,
    // (300015 P4) WebIDL prototype attributes are enumerable in Chrome
    // (getOwnPropertyNames(OAC.prototype).length desc: enumerable true) —
    // class-body getters come out non-enumerable, so sweep them too.
    globalThis.AudioBuffer, globalThis.BaseAudioContext,
    globalThis.AudioContext, globalThis.OfflineAudioContext,
    // (300015 P6) same for the conn/uad interfaces — brands/mobile/platform/
    // toJSON/getHighEntropyValues and the NetworkInformation getters are all
    // enumerable in Chrome.
    globalThis.NetworkInformation, globalThis.NavigatorUAData,
  ]) add(C);
  for (const n of Object.getOwnPropertyNames(globalThis)) {
    if (/^(HTML|SVG)[A-Za-z]*Element$/.test(n)) add(globalThis[n]);
  }
  for (const P of protos) {
    for (const k of Object.getOwnPropertyNames(P)) {
if (k === "constructor" || k.charCodeAt(0) === 95) continue; // #30: engine-internal _ members stay non-enumerable
      const d = Object.getOwnPropertyDescriptor(P, k);
      if (!d || d.enumerable || !d.configurable) continue;
      try { Object.defineProperty(P, k, { enumerable: true }); } catch (e) {}
    }
  }
})();

// (#203) WebIDL brands. Chrome answers Object.prototype.toString with the
// interface name for every platform object — '[object Event]',
// '[object HTMLDivElement]', '[object XMLHttpRequest]' — because WebIDL
// places Symbol.toStringTag on each interface prototype. The shims above
// are plain classes, so they all read '[object Object]', and page code
// that separates data from platform objects by that string misroutes:
// doudian's deep-clone helper treats '[object Object]' as "plain data,
// walk own keys", recursed into an Event (target→node→ownerDocument→…
// back-edges) and died with RangeError: Maximum call stack size exceeded,
// killing the goods-store boot. Tag every interface-shaped constructor;
// native builtins already carrying a tag are skipped, as are the
// %Object.prototype% family (Chrome leaves those untagged on purpose) and
// the legacy element factories Image/Option/Audio (instances carry the
// per-tag interface's brand instead).
(function () {
  const SKIP = new Set([
    "Object", "Function", "Array", "String", "Number", "Boolean", "Symbol",
    "BigInt", "Math", "JSON", "Image", "Option", "Audio",
  ]);
  for (const name of Object.getOwnPropertyNames(globalThis)) {
    if (!/^[A-Z][A-Za-z0-9]*$/.test(name) || SKIP.has(name)) continue;
    let C;
    try { C = globalThis[name]; } catch (e) { continue; }
    if (typeof C !== "function" || !C.prototype || typeof C.prototype !== "object") continue;
    let has;
    try { has = Object.getOwnPropertyDescriptor(C.prototype, Symbol.toStringTag) !== undefined; } catch (e) { continue; }
    if (has) continue;
    try {
      Object.defineProperty(C.prototype, Symbol.toStringTag, { value: name, enumerable: false, writable: false, configurable: true });
    } catch (e) { /* page froze the prototype first */ }
  }
  // Singleton platform objects built as literals rather than instances of
  // an exposed interface: Chrome reports '[object Console]',
  // '[object Location]', '[object History]', '[object Performance]',
  // '[object Crypto]', '[object Storage]'.
  const SINGLETONS = [
    [globalThis.console, "Console"], [globalThis.location, "Location"],
    [globalThis.history, "History"], [globalThis.performance, "Performance"],
    [globalThis.crypto, "Crypto"],
    [globalThis.localStorage, "Storage"], [globalThis.sessionStorage, "Storage"],
  ];
  for (const [obj, tag] of SINGLETONS) {
    if (!obj || obj[Symbol.toStringTag] !== undefined) continue;
    try { Object.defineProperty(obj, Symbol.toStringTag, { value: tag, enumerable: false, writable: false, configurable: true }); } catch (e) {}
  }
})();

// fp-diff-1014 tail pass: this file is the LAST module in the snapshot
// build, so by the time it runs, fetch.js/xhr.js/domparser.js/performance.js/
// window_faces.js/message_port.js have all installed their globals. The
// name sweep and the iframe frame-copy snapshot were first computed at the
// end of bootstrap.js and would otherwise stay blind to those installs
// (`globalThis.fetch` stayed nameless; iframe windows missed late globals).
try { globalThis.__diting_name_sweep && globalThis.__diting_name_sweep(); } catch (e) {}
try {
  if (globalThis.__diting_frame_globals_build) {
    globalThis.__diting_frame_globals = globalThis.__diting_frame_globals_build();
  }
} catch (e) {}

// fp-diff-1015 descriptor parity pass. Chrome exposes the classic window
// table as ACCESSORS, not writable data props: getter-only replaceable-false
// (crossOriginIsolated/crypto/navigator/... - sloppy assignment is a silent
// no-op, delete works), getter + [[Replaceable]] setter (name/dpr/self/... -
// assignment really stores, name/status coerce via ToString), and
// getter-only NON-configurable (document/top/window). The snapshot leaves
// every one of these as a plain writable data prop, so a one-line
// `typeof Object.getOwnPropertyDescriptor(window,'navigator').get` probe
// separates us from Chrome. Convert in place, parking values in a hidden
// slot bag (__ditingWinSlot - filtered by the enumeration boundary). The
// bag doubles as the rebind surface for the worker DOM mask and CDP frame
// scoping, which can no longer redefine/delete the frozen trio: they swap
// slot entries instead, with the __ditingWinSlotHide sentinel making a
// masked binding read undefined through the ordinary getters.
// MUST run after __diting_frame_globals_build above: the iframe copy set
// was captured from the pre-conversion data props, so iframe realms keep
// the historical value-prop face (iframe descriptor face = separate ledger).
try { (function () {
  var g = globalThis;
  var slot = {};
  var HIDE = {};
  g.__ditingWinSlotHide = HIDE;
  function hideAware(fn) {
    var wrapped = function () { var v = fn(); return v === HIDE ? undefined : v; };
    _markNative(wrapped);
    return wrapped;
  }
  function getterFor(k) {
    var raw = function () { return slot[k]; };
    _markNative(raw);
    return raw;
  }
  function setterFor(k, coerce) {
    var raw = coerce
      ? function (v) { slot[k] = String(v); }
      : function (v) { slot[k] = v; };
    _markNative(raw);
    return raw;
  }
  function convert(names, opts) {
    for (var i = 0; i < names.length; i++) {
      var n = names[i];
      var d = Object.getOwnPropertyDescriptor(g, n);
      // Only convert names that exist as plain data props here; accessor
      // installs (document, handled below) and absent names keep their shape.
      if (!d || d.get) continue;
      slot[n] = d.value;
      Object.defineProperty(g, n, {
        enumerable: true,
        configurable: opts.frozen ? false : true,
        get: hideAware(getterFor(n)),
        set: opts.replaceable ? setterFor(n, n === "name" || n === "status") : undefined
      });
    }
  }
  var GET_ONLY = ["crossOriginIsolated", "caches", "crypto", "customElements",
    "frameElement", "history", "indexedDB", "localStorage", "navigator",
    "sessionStorage", "speechSynthesis"];
  var GET_SET = ["devicePixelRatio", "frames", "length", "name", "onerror",
    "opener", "parent", "performance", "screen", "screenLeft", "screenTop",
    "screenX", "screenY", "self", "status", "visualViewport"];
  var GET_ONLY_FROZEN = ["top", "window"];
  convert(GET_ONLY, {});
  convert(GET_SET, { replaceable: true });
  convert(GET_ONLY_FROZEN, { frozen: true });
  // document keeps bootstrap's lazy accessor (per-page _dom resolution) as
  // its fallback: slot.document === undefined means "resolve lazily", a
  // concrete value (worker mask's HIDE sentinel, CDP frame scoping's frame
  // doc) overrides it. Chrome truth: getter-only, non-configurable.
  var ddoc = Object.getOwnPropertyDescriptor(g, "document");
  if (ddoc && ddoc.get) {
    var origDocGet = ddoc.get;
    slot.document = undefined;
    var docGet = function () {
      var v = slot.document;
      return v === HIDE ? undefined : (v !== undefined ? v : origDocGet());
    };
    _markNative(docGet);
    // accessor->accessor redefine RETAINS an omitted setter — pass
    // set: undefined explicitly or bootstrap's _documentInstance setter
    // survives and pages can clobber the binding.
    Object.defineProperty(g, "document", {
      enumerable: ddoc.enumerable === true,
      configurable: false,
      get: docGet,
      set: undefined
    });
  }
  // webkitRTCPeerConnection: Chrome keeps the legacy alias non-enumerable
  // (still in getOwnPropertyNames, absent from Object.keys).
  var wd = Object.getOwnPropertyDescriptor(g, "webkitRTCPeerConnection");
  if (wd && wd.enumerable) {
    Object.defineProperty(g, "webkitRTCPeerConnection", { enumerable: false });
  }
  g.__ditingWinSlot = slot;
})(); } catch (e) {}
