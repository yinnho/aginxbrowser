// Iframe document/window realm shims.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.

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
    // Orphan-subtree marker (obscura #976): these nodes live outside the
    // main document, so `instanceof _IframeDocument` checks from the JS side
    // need a reachable pointer — Element.ownerDocument answers the MAIN
    // document for every element, so gBCR walks up parentNode looking for
    // this stamp instead.
    this._root._isIframeDocRoot = true;
    this._root._ownerDoc = this;
    // Keep <head> content as real nodes (not just strip it for the body
    // extract): srcdoc documents run their head scripts (see
    // _runSrcdocScripts), and title/meta land in the tree where
    // querySelector finds them. innerHTML never executes scripts, so this
    // only changes DOM shape — execution stays opt-in per load path.
    var headMatch = html.match(/<head[^>]*>([\s\S]*?)<\/head>/i);
    if (headMatch && headMatch[1].trim()) {
      this._head.innerHTML = headMatch[1];
    }
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
    __hideOwn(this);
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
    // Scoped queries can never return the scope element itself, so the
    // delegated `_root.querySelector(':root')` misses the document element.
    if (typeof sel === 'string' && sel.trim() === ':root') return this._root;
    return this._root.querySelector(sel);
  }
  querySelectorAll(sel) {
    if (typeof sel === 'string' && sel.trim() === ':root') return _nodeList([this._root]);
    return this._root.querySelectorAll(sel);
  }
  // Hit testing inside the frame: real input pipelines descend into child
  // frames, so the CDP input path calls these through __diting_hitTarget.
  // The shared Document walk works unchanged here — querySelectorAll, body,
  // documentElement all delegate, and gBCR on frame elements already answers
  // in the iframe's own viewport (obscura #976 sub-run).
  elementFromPoint(x, y) {
    return Document.prototype.elementFromPoint.call(this, x, y);
  }
  elementsFromPoint(x, y) {
    return Document.prototype.elementsFromPoint.call(this, x, y);
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
    __listenerGate(listener);
    const L = __lmap(this);
    const list = L[type] || (L[type] = []);
    if (!list.includes(listener)) list.push(listener);
  }
  removeEventListener(type, listener) {
    const L = __evtStore.get(this);
    const list = L && L[type];
    if (!list) return;
    const index = list.indexOf(listener);
    if (index !== -1) list.splice(index, 1);
  }
  dispatchEvent(event) {
    const type = event && event.type;
    if (!type) return true;
    const L = __evtStore.get(this);
    const list = L && L[type];
    if (list) {
      for (const listener of list.slice()) {
        try { _invokeListener(listener, this, event); } catch (error) { console.error(error); }
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

const __frameOnHandlers = new WeakMap();
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
    // The frame's viewport IS the iframe element's laid-out box on the host
    // page (Chrome semantics; the CSS 300x150 replaced-box default stands
    // until host layout has measured it). Lazy getters: instances are
    // created per iframe at runtime, and the host box can change.
    const _vpSize = (axis) => {
      const ivp = _ditingIframeViewport(doc.documentElement);
      return axis === "w" ? ivp.w : ivp.h;
    };
    Object.defineProperty(this, 'innerWidth', { get: () => _vpSize("w"), configurable: true });
    Object.defineProperty(this, 'innerHeight', { get: () => _vpSize("h"), configurable: true });
    Object.defineProperty(this, 'outerWidth', { get: () => _vpSize("w"), configurable: true });
    Object.defineProperty(this, 'outerHeight', { get: () => _vpSize("h"), configurable: true });
    this.devicePixelRatio = globalThis.devicePixelRatio;
    this.localStorage = globalThis.localStorage;
    this.sessionStorage = globalThis.sessionStorage;
    this.performance = globalThis.performance;
    this.crypto = globalThis.crypto;
    this.console = globalThis.console;
    this.chrome = globalThis.chrome;

    if (url === 'about:srcdoc') {
      // about:srcdoc inherits the PARENT's origin (same-origin document);
      // URL() can't express that, so build the location directly.
      const pOrigin = (function() { try { return new URL(_domParse("document_url") || "about:blank").origin; } catch (e) { return ''; } })();
      this.location = {
        href: 'about:srcdoc', origin: pOrigin, protocol: 'about:',
        host: '', hostname: '', port: '', pathname: 'srcdoc', search: '', hash: '',
        toString() { return 'about:srcdoc'; }, assign() {}, reload() {}, replace() {},
      };
    } else {
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
    // fp-diff-1014 #3: a real about:blank iframe window carries the FULL
    // global set (~1235 own keys); ours exposed ~25, so one
    // getOwnPropertyNames(contentWindow) told any jsvmp this is not a real
    // browsing context. Copy the main realm's standard globals (snapshot
    // precomputed once at bootstrap end). The `in` guard covers the whole
    // prototype chain: anything _IframeWindow.prototype already implements
    // (postMessage gates on the IFRAME's origin — copying main's would gate
    // on the caller's) keeps its per-window behavior, just stays off the
    // own-key census. Identity-bearing state assigned above is covered by
    // the same guard plus the explicit skip set in the snapshot.
    const __fg = globalThis.__diting_frame_globals;
    if (__fg) {
      for (let i = 0; i < __fg.length; i++) {
        const e = __fg[i];
        if (!(e[0] in this)) {
          Object.defineProperty(this, e[0], e[1]);
        }
      }
    }
    // Chrome's iframe window owns these as own enumerable properties; the
    // `in` guard above skips them because the prototype chain already has
    // them. Anti-bot window-vs-clean-iframe diffs (xhs b1 field x82) list
    // every name that is own on the main window but not on a fresh frame.
    // Functions are shared; accessors get frame-local getters so the frame
    // never reads the main window's scroll state.
    const __ownForced = ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval',
      'requestAnimationFrame', 'isSecureContext', 'scrollX', 'scrollY', 'pageXOffset',
      'pageYOffset', 'matchMedia', 'getComputedStyle', 'getSelection', 'focus', 'blur'];
    for (let i = 0; i < __ownForced.length; i++) {
      const k = __ownForced[i];
      if (Object.prototype.hasOwnProperty.call(this, k)) continue;
      const d = Object.getOwnPropertyDescriptor(globalThis, k);
      if (!d) continue;
      try {
        if ('value' in d) {
          Object.defineProperty(this, k, d);
        } else {
          const secure = k === 'isSecureContext';
          const g = Object.getOwnPropertyDescriptor({ get [k]() {
            return secure ? d.get.call(globalThis) : 0;
          } }, k).get;
          _markNative(g);
          Object.defineProperty(this, k, { get: g, set: d.set, enumerable: d.enumerable, configurable: d.configurable });
        }
      } catch (e) {}
    }
    // Census parity: every other name the main window owns but this frame only
    // inherits (close, postMessage, on* handlers, ...) becomes an own property
    // carrying the inherited descriptor. The getter/method keeps running
    // against THIS window (`this`), so per-frame behavior is unchanged; only
    // the own-key census now matches Chrome's contentWindow.
    {
      const __mainNames = Object.getOwnPropertyNames(globalThis);
      for (let i = 0; i < __mainNames.length; i++) {
        const k = __mainNames[i];
        if (k === 'constructor' || k.startsWith('_') || k.includes('diting') || k.includes('Diting')) continue;
        if (Object.prototype.hasOwnProperty.call(this, k)) continue;
        let o = Object.getPrototypeOf(this), d = null;
        while (o && !d) { d = Object.getOwnPropertyDescriptor(o, k); o = Object.getPrototypeOf(o); }
        if (!d) {
          // Not inherited either: the main window owns it directly. Event
          // handler attributes get a frame-local accessor pair (handler kept
          // per window, native-shaped names); anything else shares the main
          // descriptor.
          const md = Object.getOwnPropertyDescriptor(globalThis, k);
          if (!md) continue;
          if (k.startsWith('on') && !('value' in md)) {
            const gs = Object.getOwnPropertyDescriptor({ get [k]() {
              const m = __frameOnHandlers.get(this); return m && m[k] !== undefined ? m[k] : null;
            } }, k).get;
            const ss = Object.getOwnPropertyDescriptor({ set [k](v) {
              let m = __frameOnHandlers.get(this); if (!m) { m = {}; __frameOnHandlers.set(this, m); }
              m[k] = (typeof v === 'function' || (v !== null && typeof v === 'object')) ? v : null;
            } }, k).set;
            _markNative(gs); _markNative(ss);
            d = { get: gs, set: ss, enumerable: md.enumerable, configurable: md.configurable };
          } else {
            d = md;
          }
        }
        try { Object.defineProperty(this, k, d); } catch (e) {}
      }
    }
    __hideOwn(this);
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
    __listenerGate(fn);
    const L = __lmap(this);
    if (!L[type]) L[type] = [];
    L[type].push(fn);
  }
  removeEventListener(type, fn) {
    const L = __evtStore.get(this);
    if (L && L[type]) {
      L[type] = L[type].filter(h => h !== fn);
    }
  }
  dispatchEvent(event) {
    const L = __evtStore.get(this);
    const handlers = (L && L[event?.type]) || [];
    for (const h of handlers) { try { _invokeListener(h, this, event); } catch(e) { globalThis.__diting_reportUncaughtError(e); } }
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

// Shadow-including "is this node in a document": `_nodeInDocument` stops at a
// ShadowRoot (its parentNode is null), which is right for window-named
// elements but wrong for browsing contexts — an iframe inside a connected
// host's shadow tree is inserted into the document and loads.
function __inDocShadowIncl(node) {
  let cur = node;
  for (let steps = 0; cur && steps < 200; steps++) {
    if (cur.nodeType === 9) return true;
    cur = cur.parentNode || (cur.nodeType === 11 ? cur._host : null);
  }
  return false;
}

// An iframe with nothing to navigate to (no src, src="about:blank", no
// srcdoc) is created on its initial about:blank document, and Chrome fires its
// load event synchronously inside every insertion — listeners attached before
// the append hear it, ones attached after do not, and a re-insertion fires
// again (a fresh browsing context). One insertion can reach this twice (a
// fragment append registers per child and again per fragment), so a repeat
// for the same parent within the same synchronous turn is swallowed.
function __iframeBlankLoad(f) {
  if (f._blankLoadedIn === f.parentNode) return;
  __def(f, '_blankLoadedIn', f.parentNode);
  queueMicrotask(() => { f._blankLoadedIn = null; });
  try { f.dispatchEvent(new Event('load')); } catch (e) { globalThis.__diting_reportUncaughtError?.(e); }
}
