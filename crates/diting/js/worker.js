// Worker family: source resolver, Worker class, worker scope boot.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.

// Shared source resolver for Worker and SharedWorker: blob-store text, blob
// object reference (readable immediately — Blob.text resolves as a microtask,
// before any setTimeout(0) macrotask), or fetch for an http(s) URL INCLUDING
// RELATIVE ones like "/workers/signals-worker.js", which WorkOS Radar and many
// SDKs use. `code` is the script text (null when it could not be resolved);
// `fetchUrl` is the http(s) URL the script came from, which is what relative
// importScripts() resolve against.
function _workerSourceFor(url) {
  if (typeof url !== 'string') return Promise.resolve({ code: null, fetchUrl: null, error: null });
  const text = globalThis.__blobStore?.[url];
  if (text !== undefined) return Promise.resolve({ code: text, fetchUrl: null, error: null });
  const obj = globalThis.__blobObjs?.[url];
  if (obj && typeof obj.text === 'function') return obj.text().then(code => ({ code, fetchUrl: null, error: null }));
  let abs = url;
  if (!/^(https?|blob|data):/i.test(url)) {
    try { abs = new URL(url, globalThis.location?.href || 'https://example.com/').href; }
    catch { return Promise.resolve({ code: null, fetchUrl: null, error: null }); }
  }
  if (/^https?:/i.test(abs)) {
    return fetch(abs).then(r => r.text())
      .then(code => ({ code, fetchUrl: abs, error: null }))
      .catch(e => ({ code: null, fetchUrl: abs, error: e }));
  }
  return Promise.resolve({ code: null, fetchUrl: null, error: null });
}

globalThis.Worker = class Worker {
  constructor(url, options) {
    this.onmessage = null;
    this.onerror = null;
    this._terminated = false;
    __lmap(this);
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
      return _workerSourceFor(url).then(res => {
        if (res.fetchUrl) worker._url = res.fetchUrl;
        if (res.error) worker._fetchError = res.error;
        return res.code;
      });
    };
    resolveCode()
      .then(code => _preloadWorkerImports(worker, code).then(() => code))
      .then(code => {
      worker._code = code;
      worker._codeReady = true;
      // A real worker executes its top-level script the moment the source
      // lands. Anti-fraud collectors boot themselves — timers, XHR probes,
      // a self-posting pump — and never wait for a parent message, so
      // deferring the whole body to the first postMessage meant those
      // workers sat dead forever (obscura#851 family).
      try { _bootWorker(worker); }
      catch (e) {
        console.error('Worker error:', e.message);
        _fireWorkerError(worker, e, __evtStore.get(worker));
      }
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
          if (code) _runWorkerMessage(worker, m.data);
          else _fireWorkerError(worker, worker._fetchError || new Error('Worker script failed to load or execute (no message from browser)'), __evtStore.get(worker));
        }
      }
    });
    __hideOwn(this);
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
        _fireWorkerError(worker, err, __evtStore.get(worker));
        return;
      }
      _runWorkerMessage(worker, data);
    };
    setTimeout(attempt, 0);
  }
  terminate() { this._terminated = true; }
  addEventListener(type, fn) {
    __listenerGate(fn);
    const L = __lmap(this);
    if (!L[type]) L[type] = [];
    L[type].push(fn);
  }
  removeEventListener(type, fn) {
    const L = __evtStore.get(this);
    if (L && L[type]) L[type] = L[type].filter(h => h !== fn);
  }
};
// importScripts() is synchronous per spec, but the only I/O this realm can
// do is promise-based while the worker body runs synchronously inside
// new Function — so the targets are preloaded: string-literal arguments in
// the worker source (and in each imported source, three waves deep) are
// fetched before _codeReady flips, and importScripts() replays them from
// the cache. Replay uses indirect eval so top-level var/const/function land
// in the global lexical scope, where the Function-wrapped worker body
// (whose scope chain ends at the global environment) and later
// importScripts() calls can see them — that is what makes
// "importScripts(lib); use lib" work. Imports share the page realm, which
// the synthetic worker already does. Arguments that are not string
// literals, or whose fetch failed, throw at call time — a real failed
// importScripts surfaces the same way (the worker's error event). The
// preloader goes through the page's own fetch, so the SSRF deny-set and
// robots/proxy policy apply to worker imports exactly as to the page.
function _workerImportUrls(src) {
  const urls = [];
  const callRe = /importScripts\s*\(([^)]*)\)/g;
  let call;
  while ((call = callRe.exec(src))) {
    const litRe = /(["'])([^"']*)\1/g;
    let lit;
    while ((lit = litRe.exec(call[1]))) urls.push(lit[2]);
  }
  return urls;
}
globalThis.__ditingWorkerImportUrls = _workerImportUrls;

function _resolveWorkerImportUrl(u, worker) {
  try {
    return new URL(u, worker._url || (globalThis.location && globalThis.location.href) || 'https://example.com/').href;
  } catch {
    return u;
  }
}

async function _preloadWorkerImports(worker, code) {
  worker._imported = {};
  if (typeof code !== 'string' || !code.includes('importScripts')) return;
  let frontier = _workerImportUrls(code);
  for (let wave = 0; wave < 3 && frontier.length; wave++) {
    const batch = frontier;
    frontier = [];
    await Promise.all(batch.map(async raw => {
      const u = _resolveWorkerImportUrl(raw, worker);
      if (u in worker._imported) return;
      try {
        const text = u.startsWith('data:')
          ? _decodeDataScriptUrl(u)
          : await fetch(u).then(r => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.text(); });
        worker._imported[u] = text;
        for (const nested of _workerImportUrls(text)) frontier.push(nested);
      } catch {
        worker._imported[u] = null;
      }
    }));
  }
}

// The WorkerGlobalScope surface that Worker and SharedWorker scopes both
// expose. Kept in one place so a global missing from the worker realm (the
// OffscreenCanvas gap that wedged WorkOS Radar, the IDB family that 抖店's SDK
// feature-detects) is fixed for both realms at once. `importScripts` is
// injected: each instance resolves imports against its own script URL and
// import cache.
function _workerSurfaceCommon(importScripts) {
    return {
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
          performance: { now: () => globalThis.performance.now(), timeOrigin: globalThis.performance?.timeOrigin || 0 },
          location: { href: (globalThis.location && globalThis.location.href) || 'https://example.com/', origin: (globalThis.location && globalThis.location.origin) || 'https://example.com', protocol: 'https:', host: (globalThis.location && globalThis.location.host) || 'example.com', hostname: (globalThis.location && globalThis.location.hostname) || 'example.com', port: '', pathname: '/', search: '', hash: '', toString() { return this.href; } },
          // Worker Navigator must mirror the page persona: collectors
          // cross-check platform/userAgent against the main-thread values.
          // WorkerNavigator includes NavigatorAutomationInformation too:
          // webdriver is defined and false there, same as the main thread (#78).
          navigator: { hardwareConcurrency: globalThis.navigator.hardwareConcurrency, userAgent: globalThis.navigator.userAgent, appVersion: globalThis.navigator.appVersion, platform: globalThis.navigator.platform, language: globalThis.navigator.language, languages: globalThis.navigator.languages, deviceMemory: globalThis.navigator.deviceMemory, webdriver: false, onLine: true },
          Request: globalThis.Request, Response: globalThis.Response,
          Headers: globalThis.Headers, Blob: globalThis.Blob,
          FormData: globalThis.FormData, URL: globalThis.URL,
          URLSearchParams: globalThis.URLSearchParams,
          AbortController: globalThis.AbortController, AbortSignal: globalThis.AbortSignal,
          WebSocket: globalThis.WebSocket, Event: globalThis.Event,
          MessageEvent: globalThis.MessageEvent, ErrorEvent: globalThis.ErrorEvent,
          MessageChannel: globalThis.MessageChannel, MessagePort: globalThis.MessagePort,
          indexedDB: globalThis.indexedDB,
          IDBFactory: globalThis.IDBFactory, IDBRequest: globalThis.IDBRequest,
          IDBOpenDBRequest: globalThis.IDBOpenDBRequest, IDBDatabase: globalThis.IDBDatabase,
          IDBTransaction: globalThis.IDBTransaction, IDBObjectStore: globalThis.IDBObjectStore,
          IDBIndex: globalThis.IDBIndex, IDBCursor: globalThis.IDBCursor,
          IDBCursorWithValue: globalThis.IDBCursorWithValue, IDBKeyRange: globalThis.IDBKeyRange,
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
          importScripts,
          queueMicrotask: globalThis.queueMicrotask,
          structuredClone: globalThis.structuredClone,
        };
}

// (#98) Chrome's worker scopes have no DOM at all: document,
// MutationObserver, the element classes are undefined there. The
// `with (self)` wrappers below only shadow names the scope object carries
// — everything else falls through to the page realm, so PDD's
// sharedWorker polyfill saw the page's MutationObserver, picked its DOM
// scheduling branch (observe a text node) and died on the undefined
// `document` parameter. The proxy claims ownership of the main-thread-only
// family so those lookups stop at the scope and read undefined, exactly
// like Chrome; every other name keeps the fallthrough-to-page behavior.
const _WORKER_MAIN_ONLY = new Set([
  'document', 'window', 'top', 'parent', 'frames',
  'MutationObserver', 'WebKitMutationObserver', 'ResizeObserver', 'IntersectionObserver',
  'DOMParser', 'XMLSerializer', 'localStorage', 'sessionStorage',
  'getComputedStyle', 'getSelection', 'requestAnimationFrame', 'cancelAnimationFrame',
  'alert', 'confirm', 'prompt', 'print', 'history', 'screen', 'visualViewport',
  'Image', 'Audio', 'Option', 'customElements', 'trustedTypes',
  'Node', 'Element', 'HTMLElement', 'SVGElement', 'Document', 'DocumentFragment', 'ShadowRoot',
]);
function _workerScopeChain(scope) {
    return new Proxy(scope, {
      has(t, p) { return _WORKER_MAIN_ONLY.has(p) ? true : (p in t); },
      get(t, p) { return _WORKER_MAIN_ONLY.has(p) ? undefined : t[p]; },
      set(t, p, v) { t[p] = v; return true; },
    });
}

// Mask the main-thread-only family on the page global for the duration of
// one synchronous worker execution — Chrome runs worker code in its own
// realm where no DOM exists; ours shares the page realm, so the wheel's
// globalThis reads (webpack's global helper) must be masked here, where no
// with-scope can reach. Configurable properties swap descriptors
// (accessors included); writable-but-locked ones take a plain undefined
// assignment; the frozen window-table trio (document/top/window — Chrome
// truth is non-configurable) masks through the __ditingWinSlot bag their
// getters read; anything else stays visible (degraded, not broken). Nesting
// is reference-counted by the outermost window: an inner mask while one is
// live is a no-op.
const _mainMaskSaved = { list: null };
function _restoreMainMask(saved) {
    for (const [name, d, v, viaSlot] of saved) {
      try {
        if (viaSlot) globalThis.__ditingWinSlot[name] = v;
        else if (d) Object.defineProperty(globalThis, name, d);
        else globalThis[name] = v;
      } catch (_) {}
    }
}
function _applyMainMask(saved) {
    for (const [name, d, , viaSlot] of saved) {
      try {
        if (viaSlot) globalThis.__ditingWinSlot[name] = globalThis.__ditingWinSlotHide;
        else if (d) Object.defineProperty(globalThis, name, { value: undefined, writable: true, configurable: true, enumerable: false });
        else globalThis[name] = undefined;
      } catch (_) {}
    }
}
function _maskMainThreadGlobals() {
    if (_mainMaskSaved.list) return () => {};
    const saved = [];
    for (const name of _WORKER_MAIN_ONLY) {
      const d = Object.getOwnPropertyDescriptor(globalThis, name);
      if (!d) continue;
      if (d.configurable) {
        saved.push([name, d, undefined, false]);
      } else if (d.writable) {
        saved.push([name, null, d.value, false]);
      } else if (globalThis.__ditingWinSlot && (name in globalThis.__ditingWinSlot)) {
        // Frozen window-table trio (document/top/window): the getter reads
        // the slot bag, so mask and restore through it.
        saved.push([name, d, globalThis.__ditingWinSlot[name], true]);
      } else {
        continue;
      }
    }
    _applyMainMask(saved);
    _mainMaskSaved.list = saved;
    return () => { _mainMaskSaved.list = null; _restoreMainMask(saved); };
}
// Page-code entry from inside a worker dispatch: the dedicated worker's
// synchronous postMessage shim hands control straight to page handlers,
// which must see the full page global. Returns a callback that reinstates
// the worker mask once the page callbacks return.
function _unmaskMainThreadGlobals() {
    if (!_mainMaskSaved.list) return () => {};
    const saved = _mainMaskSaved.list;
    _mainMaskSaved.list = null;
    _restoreMainMask(saved);
    return () => { _applyMainMask(saved); _mainMaskSaved.list = saved; };
}

// The importScripts() body shared by both realms: replay preloaded sources
// (see _preloadWorkerImports) in order through indirect eval so top-level
// declarations land in the global lexical scope. (#98) Chrome replays
// imports in the worker's own global, where no DOM exists — ours replays
// into the page realm, where PDD's wheel saw the page's MutationObserver,
// picked its DOM scheduling branch and threw on document.createTextNode.
// The mask above gives the payload Chrome's worker reality for the
// synchronous eval window; declarations keep landing where the Function-
// wrapped body can see them.
function _workerImportScripts(importShim) {
  return (...urls) => {
    for (const raw of urls) {
      const u = _resolveWorkerImportUrl(raw, importShim);
      const text = importShim._imported && importShim._imported[u];
      if (typeof text !== 'string') {
        throw new Error('importScripts: failed to load "' + raw + '"');
      }
      const restore = _maskMainThreadGlobals();
      try { (0, eval)(text); }
      finally { restore(); }
    }
  };
}

// Build the worker scope and execute the body's top level. Idempotent —
// the constructor calls it once the source is ready, and every delivered
// message lands here first.
function _bootWorker(worker) {
  if (worker._workerSelf) return;
    const workerSelf = _workerSurfaceCommon(_workerImportScripts(worker));
    // `onmessage` is pre-declared so the `with (self)` wrapper below catches a
    // bare `onmessage = fn` and lands it on the worker scope.
    workerSelf.onmessage = null;
    workerSelf.postMessage = (msg) => {
      const evt = { data: msg };
      // Page handlers run with the page global fully intact even when the
      // worker is mid-dispatch under the mask.
      const reMask = _unmaskMainThreadGlobals();
      try {
        if (worker.onmessage) worker.onmessage(evt);
        const wl = __evtStore.get(worker);
        const handlers = (wl && wl['message']) || [];
        for (const h of handlers) _invokeListener(h, worker, evt);
      } finally { reMask(); }
    };
    workerSelf.addEventListener = (type, fn) => { workerSelf['on' + type] = fn; };
    workerSelf.removeEventListener = () => {};
    workerSelf.close = () => { worker._terminated = true; };
        workerSelf.self = workerSelf;
        worker._workerSelf = workerSelf;
        // `with` puts the worker scope object in front of the scope chain so
        // a bare `onmessage = fn` (no self. prefix) lands on the worker scope
        // like in a real DedicatedWorkerGlobalScope — a plain Function body
        // let the assignment fall through to the page global and the worker
        // never received anything (obscura#867 family).
        const fn = new Function('self', 'postMessage', 'addEventListener', 'removeEventListener', 'close', 'window', 'document', 'navigator', 'location', 'importScripts',
          'with (self) {\n' + worker._code + '\n}');
        const restore = _maskMainThreadGlobals();
        try { fn(_workerScopeChain(workerSelf), workerSelf.postMessage, workerSelf.addEventListener, workerSelf.removeEventListener, workerSelf.close, undefined, undefined, workerSelf.navigator, workerSelf.location, workerSelf.importScripts); }
        finally { restore(); }
}

function _runWorkerMessage(worker, data) {
  const restore = _maskMainThreadGlobals();
  try {
    _bootWorker(worker);
    if (worker._workerSelf.onmessage) worker._workerSelf.onmessage({ data });
  } catch(e) {
    console.error('Worker error:', e.message);
    _fireWorkerError(worker, e, __evtStore.get(worker));
  } finally { restore(); }
}

// Module-level so both construction-time fetch failures and execution-time
// throws share one ErrorEvent-shaped path.
function _fireWorkerError(worker, err, listeners) {
  const message = err instanceof Error ? (err.message || err.name) : String(err);
  const evt = { message, filename: '', lineno: 0, colno: 0, error: err };
  const handlers = ((listeners && listeners['error']) || []).slice();
  if (worker.onerror) handlers.unshift(worker.onerror);
  for (const h of handlers) { try { _invokeListener(h, worker, evt); } catch (e) { globalThis.__diting_reportUncaughtError(e); } }
}
