// Navigation API (split from bootstrap.js): `navigation` with entries /
// currentEntry / navigate / reload / back / forward / traverseTo /
// updateCurrentEntry, the navigate (NavigateEvent.intercept) /
// currententrychange / navigatesuccess / navigateerror events, layered on the
// session-history stack bootstrap's `history` owns (read through
// __diting_history). Link clicks and location.assign/replace consult
// __navigationIntercept, so a router that intercepts same-origin navigations
// keeps the document alive. Dormant (zero cost) until a navigate listener exists.
(() => {
  const H = () => globalThis.__diting_history();
  const meta = new WeakMap();      // history stack entry -> {key,id,state}
  const wrappers = new WeakMap();  // history stack entry -> NavigationHistoryEntry
  let navListeners = 0;
  const uuid = () => (crypto.randomUUID ? crypto.randomUUID() : String(Math.random()).slice(2));
  const metaOf = (e) => { let m = meta.get(e); if (!m) { m = { key: uuid(), id: uuid(), state: undefined }; meta.set(e, m); } return m; };
  const ET = globalThis.EventTarget;
  const chain = (C, P) => { if (C && P && Object.getPrototypeOf(C.prototype) !== P.prototype) { Object.setPrototypeOf(C.prototype, P.prototype); Object.setPrototypeOf(C, P); } };
  chain(globalThis.Navigation, ET); chain(globalThis.NavigationHistoryEntry, ET);
  chain(globalThis.NavigateEvent, globalThis.Event); chain(globalThis.NavigationCurrentEntryChangeEvent, globalThis.Event);
  const curUrl = () => { try { return new URL(globalThis.location.href); } catch (e) { return null; } };
  const entryUrl = (e) => { if (e.url) return e.url; return globalThis.location.href; };

  const mkEntry = (stackEntry) => {
    if (!stackEntry) return null;
    let w = wrappers.get(stackEntry);
    if (w) return w;
    w = new ET();
    Object.setPrototypeOf(w, (globalThis.NavigationHistoryEntry || { prototype: ET.prototype }).prototype);
    const def = (n, g) => Object.defineProperty(w, n, { get: g, enumerable: true, configurable: true });
    def('url', () => (stackEntry.cross ? null : entryUrl(stackEntry)));
    def('key', () => metaOf(stackEntry).key);
    def('id', () => metaOf(stackEntry).id);
    def('index', () => H().stack.indexOf(stackEntry));
    def('sameDocument', () => !stackEntry.cross);
    Object.defineProperty(w, 'getState', { value() { const s = metaOf(stackEntry).state; return s === undefined ? undefined : structuredClone(s); }, configurable: true, writable: true });
    wrappers.set(stackEntry, w);
    return w;
  };

  const nav = new ET();
  Object.setPrototypeOf(nav, (globalThis.Navigation || { prototype: ET.prototype }).prototype);
  const addL = nav.addEventListener.bind(nav);
  Object.defineProperty(nav, 'addEventListener', { value(type, ...rest) { if (type === 'navigate') navListeners++; return addL(type, ...rest); }, configurable: true, writable: true });
  let transition = null;
  const dm = (name, msg) => new DOMException(msg, name);
  const prop = (n, g) => Object.defineProperty(nav, n, { get: g, enumerable: true, configurable: true });
  prop('currentEntry', () => { const h = H(); return mkEntry(h.stack[h.index]); });
  prop('canGoBack', () => H().index > 0);
  prop('canGoForward', () => { const h = H(); return h.index < h.stack.length - 1; });
  prop('transition', () => transition);
  prop('activation', () => null);
  for (const n of ['onnavigate', 'onnavigatesuccess', 'onnavigateerror', 'oncurrententrychange']) {
    let fn = null;
    Object.defineProperty(nav, n, { get() { return fn; }, set(v) { if (fn) nav.removeEventListener(n.slice(2), fn); fn = typeof v === 'function' ? v : null; if (fn) nav.addEventListener(n.slice(2), fn); }, enumerable: true, configurable: true });
  }
  const method = (n, f) => Object.defineProperty(nav, n, { value: f, writable: true, configurable: true });
  method('entries', () => H().stack.map(mkEntry));

  const sameOrigin = (u) => { const c = curUrl(); return !!c && c.origin !== 'null' && u.origin === c.origin && u.protocol === c.protocol; };
  const isHashChange = (u) => { const c = curUrl(); return !!c && u.hash !== c.hash && u.href.split('#')[0] === c.href.split('#')[0]; };

  // Fire `navigate` synchronously and report what the listeners did.
  const begin = (type, urlObj, opts, destInfo) => {
    const handlers = [];
    let intercepted = false;
    const sameDoc = urlObj.href.split('#')[0] === globalThis.location.href.split('#')[0];
    const canIntercept = sameOrigin(urlObj) || (urlObj.protocol === globalThis.location.protocol && sameDoc);
    const ac = new AbortController();
    const dest = Object.create((globalThis.NavigationDestination || Object).prototype);
    Object.defineProperties(dest, {
      url: { value: urlObj.href, enumerable: true },
      key: { value: destInfo.key ?? null, enumerable: true },
      id: { value: destInfo.id ?? null, enumerable: true },
      index: { value: destInfo.index ?? -1, enumerable: true },
      sameDocument: { value: destInfo.sameDocument, enumerable: true },
      getState: { value() { return destInfo.state === undefined ? undefined : structuredClone(destInfo.state); } },
    });
    const ev = new Event('navigate', { cancelable: true });
    Object.setPrototypeOf(ev, (globalThis.NavigateEvent || { prototype: Event.prototype }).prototype);
    const fields = { navigationType: type, destination: dest, canIntercept, userInitiated: !!opts.userInitiated, hashChange: isHashChange(urlObj), signal: ac.signal, formData: null, downloadRequest: null, info: opts.info, hasUAVisualTransition: false, sourceElement: opts.sourceElement || null };
    for (const k of Object.keys(fields)) Object.defineProperty(ev, k, { value: fields[k], enumerable: true, configurable: true });
    Object.defineProperties(ev, {
      intercept: { value(o) {
        if (!canIntercept) throw dm('SecurityError', "Failed to execute 'intercept' on 'NavigateEvent': A navigation with URL '" + urlObj.href + "' cannot be intercepted by a document with URL '" + globalThis.location.href + "'.");
        if (ev.defaultPrevented) throw dm('InvalidStateError', "Failed to execute 'intercept' on 'NavigateEvent': The event has already been canceled.");
        intercepted = true;
        if (o && typeof o.handler === 'function') handlers.push(o.handler);
      } },
      scroll: { value() {} },
    });
    const notPrevented = nav.dispatchEvent(ev);
    return { prevented: !notPrevented, intercepted, handlers, abort: () => ac.abort(), sameDoc };
  };

  // Run one navigation: decide, commit synchronously, report through the
  // committed/finished promises. `handled` tells location-style callers
  // whether the document consumed the navigation.
  const run = (type, urlObj, opts, destInfo, commit) => {
    let resolveC, rejectC, resolveF, rejectF;
    const committed = new Promise((a, b) => { resolveC = a; rejectC = b; });
    const finished = new Promise((a, b) => { resolveF = a; rejectF = b; });
    committed.catch(() => {}); finished.catch(() => {});
    const d = begin(type, urlObj, opts, destInfo);
    if (d.prevented) {
      d.abort();
      const err = dm('AbortError', 'Navigation was aborted');
      Promise.resolve().then(() => { fire('navigateerror', { error: err }); rejectC(err); rejectF(err); });
      return { handled: true, committed, finished };
    }
    commit(d.intercepted);
    const entry = nav.currentEntry;
    resolveC(entry);
    if (!d.intercepted) {
      if (d.sameDoc) Promise.resolve().then(() => { fire('navigatesuccess', {}); resolveF(nav.currentEntry); });
      return { handled: false, committed, finished };
    }
    transition = { navigationType: type, from: null, committed: Promise.resolve(), finished };
    Promise.all(d.handlers.map((h) => { try { return Promise.resolve(h()); } catch (x) { return Promise.reject(x); } })).then(
      () => { transition = null; fire('navigatesuccess', {}); resolveF(nav.currentEntry); },
      (err) => { transition = null; fire('navigateerror', { error: err }); rejectF(err); },
    );
    return { handled: true, committed, finished };
  };
  const fire = (type, init, proto) => {
    const ev = new Event(type);
    if (proto) Object.setPrototypeOf(ev, proto.prototype);
    if (init) for (const k of Object.keys(init)) Object.defineProperty(ev, k, { value: init[k], enumerable: true, configurable: true });
    nav.dispatchEvent(ev);
  };
  const realPush = globalThis.history.pushState, realReplace = globalThis.history.replaceState, realGo = globalThis.history.go;
  const pushOrReplace = (type, state, url) => {
    (type === 'replace' ? realReplace : realPush).call(globalThis.history, undefined, '', url);
    const cur = H();
    metaOf(cur.stack[cur.index]).state = state;
  };
  const entryChange = (type, from) => fire('currententrychange', { navigationType: type, from }, globalThis.NavigationCurrentEntryChangeEvent);

  method('navigate', function navigate(url, options) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'navigate' on 'Navigation': 1 argument required, but only 0 present.");
    const o = options || {};
    let u;
    try { u = new URL(String(url), globalThis.location.href); } catch (e) { throw dm('SyntaxError', "Failed to execute 'navigate' on 'Navigation': Invalid URL '" + url + "'."); }
    // history:'auto' pushes, except when the URL is the current one or the
    // document has not finished loading (HTML navigate algorithm).
    let type = o.history === 'replace' ? 'replace' : 'push';
    if ((o.history === undefined || o.history === 'auto') && (u.href === globalThis.location.href || document.readyState !== 'complete')) type = 'replace';
    const sameDoc = u.href.split('#')[0] === globalThis.location.href.split('#')[0];
    const { committed, finished } = run(type, u, o, { sameDocument: sameDoc, state: o.state }, (intercepted) => {
      if (intercepted || sameDoc) {
        const from = nav.currentEntry;
        pushOrReplace(type, o.state, u.href);
        entryChange(type, from);
      } else {
        globalThis.__diting_locNavRaw(u.href, type === 'replace');
      }
    });
    return { committed, finished };
  });
  method('reload', function reload(options) {
    const o = options || {};
    const { committed, finished } = run('reload', curUrl(), o, { sameDocument: true, state: o.state }, (intercepted) => {
      if (o.state !== undefined) { const h = H(); metaOf(h.stack[h.index]).state = o.state; }
      if (!intercepted) globalThis.location.reload();
    });
    return { committed, finished };
  });
  const traverse = (dest, options) => {
    const o = options || {};
    const idx = H().stack.indexOf(dest);
    const u = new URL(entryUrl(dest), globalThis.location.href);
    const { committed, finished } = run('traverse', u, o, { sameDocument: !dest.cross, key: metaOf(dest).key, id: metaOf(dest).id, index: idx, state: metaOf(dest).state }, () => {
      const from = nav.currentEntry;
      realGo.call(globalThis.history, idx - H().index);
      entryChange('traverse', from);
    });
    return { committed, finished };
  };
  method('traverseTo', function traverseTo(key, options) {
    const target = H().stack.find((e) => metaOf(e).key === String(key));
    if (!target) throw dm('InvalidStateError', "Failed to execute 'traverseTo' on 'Navigation': Invalid key");
    return traverse(target, options);
  });
  method('back', function back(options) {
    const h = H();
    if (h.index <= 0) throw dm('InvalidStateError', "Failed to execute 'back' on 'Navigation': Cannot go back");
    return traverse(h.stack[h.index - 1], options);
  });
  method('forward', function forward(options) {
    const h = H();
    if (h.index >= h.stack.length - 1) throw dm('InvalidStateError', "Failed to execute 'forward' on 'Navigation': Cannot go forward");
    return traverse(h.stack[h.index + 1], options);
  });
  method('updateCurrentEntry', function updateCurrentEntry(options) {
    const h = H();
    metaOf(h.stack[h.index]).state = options && options.state;
    entryChange(null, nav.currentEntry);
  });
  method('getEntries', () => nav.entries());

  // ---- keep the entry list honest when pages use the History API directly
  const wrapHist = (name, type) => {
    const orig = globalThis.history[name];
    Object.defineProperty(globalThis.history, name, {
      value: function (...args) {
        const from = navListeners || navEntryListeners ? nav.currentEntry : null;
        const r = orig.apply(this, args);
        if (from) entryChange(type, from);
        return r;
      },
      writable: true, configurable: true,
    });
    _markNative(globalThis.history[name]);
  };
  let navEntryListeners = 0;
  const addL2 = nav.addEventListener;
  Object.defineProperty(nav, 'addEventListener', { value(type, ...rest) { if (type === 'currententrychange') navEntryListeners++; return addL2.call(nav, type, ...rest); }, configurable: true, writable: true });
  wrapHist('pushState', 'push'); wrapHist('replaceState', 'replace');
  // Anchor clicks / location.assign / location.replace consult this first;
  // true = the document consumed the navigation (canceled or intercepted).
  globalThis.__navigationIntercept = (href, replace) => {
    if (!navListeners) return false;
    let u;
    try { u = new URL(href, globalThis.location.href); } catch (e) { return false; }
    const type = replace ? 'replace' : 'push';
    const sameDoc = u.href.split('#')[0] === globalThis.location.href.split('#')[0];
    return run(type, u, {}, { sameDocument: sameDoc }, (intercepted) => {
      if (intercepted) {
        const from = nav.currentEntry;
        pushOrReplace(type, undefined, u.href);
        entryChange(type, from);
      }
    }).handled;
  };
  Object.defineProperty(globalThis, 'navigation', { get() { return nav; }, enumerable: true, configurable: true });
})();
