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
