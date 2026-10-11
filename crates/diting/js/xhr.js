// (#236 absorption split) The XMLHttpRequest subclass, moved out of
// bootstrap.js (god-file ratchet: the file only shrinks). Runs right after
// bootstrap.js in the same realm — execute_script calls concatenate — so the
// XMLHttpRequestEventTarget base (left in bootstrap: AbortSignal extends it
// mid-file at class-definition time), the engine's fetch shim closure
// (__ditingFetchShim, #235: XHR rides it, never the page-overridable fetch)
// and the _initiatorHint channel (#126) are the same globals the in-file
// implementation always used.

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
    this._timeoutTimer = null;
    this._timedOut = false;
    this._aborted = false;
    __lmap(this);
    this.onreadystatechange = null;
    this.onload = null;
    this.onerror = null;
    this.onabort = null;
    this.onprogress = null;
    this.ontimeout = null;
    this.onloadstart = null;
    this.onloadend = null;
    __hideOwn(this);
  }

  open(method, url, async_) {
    // Fetch-spec "normalize a method": a token that byte-uppercases to a
    // standard method is sent uppercased; custom tokens ride as authored.
    // Pages hand us lowercase ('get'/'put' from the Ali SDK) and the network
    // layer compares methods case-sensitively, so passthrough broke CORS
    // safelist matching and Allow-Methods checks (taobao report ⑤).
    const up = String(method).toUpperCase();
    this._method = /^(CONNECT|DELETE|GET|HEAD|OPTIONS|POST|PUT|TRACE)$/.test(up) ? up : String(method);
    // WebIDL USVString coercion: open(undefined) stores "undefined" in Chrome,
    // never a non-string — send() used to crash on .startsWith (#106).
    this._url = String(url);
    // obscura#908: open()'s third argument decides whether send() blocks for
    // the response or resolves it through the event loop.
    this._async = async_ === undefined ? true : !!async_;
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

  overrideMimeType(mime) { __def(this, '_overrideMime', mime); }

  send(body) {
    if (this.readyState !== 1) return;
    if (this._aborted) return;

    const xhr = this;

    let url = this._url;
    // (#106) guard the crash site: an open()'d XHR whose _url got cleared or
    // never set must fail like Chrome (InvalidStateError), not die on
    // url.startsWith below.
    if (typeof url !== 'string' || url === '') {
      throw new DOMException("Failed to execute 'send' on 'XMLHttpRequest': The object may not be sent yet.", "InvalidStateError");
    }
    if (url && !url.includes('://')) {
      try {
        const base = _docBase();
        url = new URL(url, base).href;
      } catch(e) {}
    }

    // (#42) data:/blob: resolve locally, not through the network ops — the
    // sync path would otherwise fall into the JSON.parse catch and zero
    // status. Chrome reports 200 + content-type for both; responseURL keeps
    // the original URL. Sync fills state inline with no events (same
    // semantics as the network sync path); async defers to a task so the
    // usual readystatechange/load/loadend sequence stays ordered.
    if (url.startsWith('data:') || url.startsWith('blob:')) {
      // #126: local resolution still owns a resource-timing entry (Chrome
      // records XHR'd data:/blob: URLs as xmlhttprequest). Async runs the
      // record inside the deferred task, so the duration includes the real
      // timer hop — an honest measurement, not the decode alone.
      const _rtT0 = globalThis.performance ? performance.now() : 0;
      const applyLocal = function () {
        let bytes = null;
        let ctype = '';
        try {
          if (url.startsWith('blob:')) {
            const b = globalThis.__blobObjs && globalThis.__blobObjs[url];
            if (b && b._bytes instanceof Uint8Array) { bytes = b._bytes; ctype = b.type || ''; }
          } else {
            const d = _dataUrlBytes(url);
            bytes = d.bytes;
            ctype = d.mime;
          }
        } catch (e) { bytes = null; }
        if (!bytes) {
          __recordFetchTiming(_rtT0, url, 'xmlhttprequest', null);
          xhr.status = 0;
          xhr.statusText = '';
          xhr._responseHeaders = {};
          xhr.responseText = '';
          xhr.response = '';
          if (xhr._async) {
            xhr._setReadyState(4);
            xhr._fireEvent('error');
            xhr._fireEvent('loadend');
          } else {
            xhr.readyState = 4;
          }
          return;
        }
        xhr.responseURL = url;
        xhr.status = 200;
        xhr.statusText = '';
        xhr._responseHeaders = ctype ? { 'content-type': ctype } : {};
        __recordFetchTiming(_rtT0, url, 'xmlhttprequest', { status: 200 }, {
          encodedBodySize: bytes.length, decodedBodySize: bytes.length, transferSize: 0,
        });
        const text = _decodeBodyWithCharset(bytes, {
          get: (name) => {
            const lower = String(name).toLowerCase();
            for (const [k, v] of Object.entries(xhr._responseHeaders)) {
              if (k.toLowerCase() === lower) return v;
            }
            return null;
          },
        });
        xhr.responseText = text;
        switch (xhr.responseType) {
          case 'json':
            try { xhr.response = JSON.parse(text); } catch(e) { xhr.response = null; }
            break;
          case 'text':
          case '':
            xhr.response = text;
            break;
          case 'arraybuffer':
            xhr.response = bytes.slice().buffer;
            break;
          case 'blob':
            xhr.response = new Blob([bytes]);
            break;
          case 'document':
            xhr.response = text; // simplified
            break;
          default:
            xhr.response = text;
        }
        if (xhr._async) {
          xhr._setReadyState(4);
          xhr._fireEvent('load');
          xhr._fireEvent('loadend');
        } else {
          xhr.readyState = 4;
        }
      };
      if (this._async) { setTimeout(applyLocal, 0); } else { applyLocal(); }
      return;
    }

    if (this._async === false) {
      // Sync XHR (obscura#908): the request must complete inside send() —
      // async ops only resolve when the embedding pumps the event loop after
      // the eval returns, which can never happen while JS holds the thread
      // here. The sync op runs the identical server-side request walk on a
      // worker thread and blocks. Spec: no events fire in the sync path
      // (not even loadstart); the caller reads status/response inline.
      // CDP Fetch interception has no sync story for the same reason.
      // #116: a non-zero timeout on sync XHR throws, like Chrome — there is
      // no event loop turn to fire it on.
      if (this.timeout > 0) {
        throw new DOMException("Failed to execute 'send' on 'XMLHttpRequest': Synchronous requests should not set a timeout.", "InvalidAccessError");
      }
      try {
        const hdrs = {};
        let bodyStr = '';
        if (body !== undefined && body !== null) {
          if (typeof body === 'string') {
            bodyStr = body;
          } else if (body instanceof ArrayBuffer) {
            bodyStr = _bytesToBase64(new Uint8Array(body));
            hdrs['__diting_body_b64'] = '1';
          } else if (ArrayBuffer.isView(body)) {
            bodyStr = _bytesToBase64(new Uint8Array(body.buffer, body.byteOffset, body.byteLength));
            hdrs['__diting_body_b64'] = '1';
          } else if (body instanceof Blob) {
            // v1: Blob bodies ride the async path only.
            console.warn('Synchronous XHR does not support Blob bodies');
            xhr.status = 0;
            xhr.responseText = '';
            xhr.response = '';
            xhr.readyState = 4;
            return;
          } else {
            bodyStr = String(body);
          }
        }
        for (const [k, v] of Object.entries(this._headers)) hdrs[k] = String(v);
        let pageOrigin = '';
        try { pageOrigin = new URL(_docBase()).origin; } catch(e) {}
        const _rtT0 = globalThis.performance ? performance.now() : 0;
        const raw = _OPS.op_fetch_url_sync(url, this._method, JSON.stringify(hdrs), bodyStr, pageOrigin, 'cors', this.withCredentials ? 'include' : 'same-origin');
        const parsed = JSON.parse(raw);
        __recordFetchTiming(_rtT0, url, 'xmlhttprequest', parsed);
        xhr.responseURL = parsed.final_url || parsed.url || url;
        if (parsed.blocked || parsed.corsBlocked) {
          xhr.status = 0;
          xhr.statusText = '';
          xhr._responseHeaders = {};
          xhr.responseText = '';
          xhr.response = '';
          xhr.readyState = 4;
          // Chrome pins (headless, 2026-10-11): a sync XHR network error
          // surfaces as a NetworkError DOMException with readyState already
          // DONE and status 0 — not a silent return. SDKs that branch on
          // try/catch (PDD SDK.sync face, #249) read the silent form as
          // success-shaped.
          throw new DOMException("A network error occurred.", "NetworkError");
        }
        xhr.status = parsed.status;
        xhr.statusText = '';
        xhr._responseHeaders = parsed.headers || {};
        const bytes = _base64ToUint8Array(parsed.bodyBase64 || '');
        const text = _decodeBodyWithCharset(bytes, {
          get: (name) => {
            const lower = String(name).toLowerCase();
            for (const [k, v] of Object.entries(xhr._responseHeaders)) {
              if (k.toLowerCase() === lower) return v;
            }
            return null;
          },
        });
        xhr.responseText = text;
        switch (xhr.responseType) {
          case 'json':
            try { xhr.response = JSON.parse(text); } catch(e) { xhr.response = null; }
            break;
          case 'text':
          case '':
            xhr.response = text;
            break;
          case 'arraybuffer':
            xhr.response = bytes.slice().buffer;
            break;
          case 'blob':
            xhr.response = new Blob([bytes]);
            break;
          case 'document':
            xhr.response = text; // simplified
            break;
          default:
            xhr.response = text;
        }
        xhr.readyState = 4;
      } catch (e) {
        xhr.status = 0;
        xhr.statusText = '';
        xhr.responseText = '';
        xhr.response = '';
        xhr.readyState = 4;
        // Same Chrome face as the blocked branch above: send() throws
        // NetworkError once state is DONE, responseText empty.
        throw new DOMException("A network error occurred.", "NetworkError");
      }
      return;
    }

    this._fireEvent('loadstart');

    // #116: xhr.timeout must race the whole send→done span (headers AND body).
    // Chrome: deadline hit → `timeout` event, DONE, status 0, then loadend;
    // a late settlement of the underlying fetch is ignored. A hung transport
    // used to fire nothing at all, which read on the surface as a silent
    // EVAL_TIMEOUT with zero events (tmall publish report).
    if (this.timeout > 0) {
      const xhr = this;
      this._timeoutTimer = setTimeout(function () {
        if (xhr._aborted || xhr.readyState === 4) return;
        xhr._timedOut = true;
        xhr._aborted = true; // then/catch heads ignore the late settlement
        xhr.status = 0;
        xhr.statusText = '';
        xhr._responseHeaders = {};
        xhr.responseText = '';
        xhr.response = '';
        // _setReadyState, not a raw assignment + _fireEvent: _fireEvent
        // deliberately skips the onreadystatechange property, so a raw DONE
        // here leaves property-awaiting code waiting forever (#248).
        xhr._setReadyState(4);
        xhr._fireEvent('timeout');
        xhr._fireEvent('loadend');
      }, this.timeout);
    }

    // #126: one record, the XHR's initiator — the hint is consumed at the
    // fetch shim's synchronous entry, before the first await.
    _initiatorHint = 'xmlhttprequest';
    // #235: the engine's own fetch shim, NOT the page-visible fetch — a
    // page that wraps window.fetch must not see (and re-process) native
    // XHR traffic through its wrapper.
    const delegated = __ditingFetchShim(url, {
      method: this._method,
      headers: this._headers,
      body: body || undefined,
      mode: 'cors',
      credentials: this.withCredentials ? 'include' : 'same-origin',
    });
    _initiatorHint = null;
    delegated.then(async (resp) => {
      if (xhr._aborted) return;

      xhr.status = resp.status;
      xhr.statusText = resp.statusText || '';
      xhr.responseURL = resp.url || url;

      if (resp.headers) {
        resp.headers.forEach((v, k) => { xhr._responseHeaders[k] = v; });
      }

      xhr._setReadyState(2); // HEADERS_RECEIVED

      // arraybuffer/blob must round-trip the raw bytes: resp.text() is a
      // lossy UTF-8 decode and re-encoding it mangles binary payloads
      // (obscura #754/#716 class). Take the byte-exact buffer once and
      // derive the charset-decoded text from the same bytes.
      const bodyBuf = await resp.arrayBuffer();
      const text = _decodeBodyWithCharset(new Uint8Array(bodyBuf), resp.headers);
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
          xhr.response = bodyBuf;
          break;
        case 'blob':
          xhr.response = new Blob([bodyBuf]);
          break;
        case 'document':
          xhr.response = text; // simplified
          break;
        default:
          xhr.response = text;
      }

      xhr._setReadyState(4); // DONE
      if (xhr._timeoutTimer) { clearTimeout(xhr._timeoutTimer); xhr._timeoutTimer = null; }
      xhr._fireEvent('load');
      xhr._fireEvent('loadend');
    }).catch((err) => {
      if (xhr._timeoutTimer) { clearTimeout(xhr._timeoutTimer); xhr._timeoutTimer = null; }
      if (xhr._aborted) return;
      xhr.status = 0;
      // _setReadyState so property onreadystatechange sees DONE too —
      // _fireEvent skips that property on purpose (#248)
      xhr._setReadyState(4);
      if (err && err.__aborted) {
        xhr._aborted = true;
        xhr._fireEvent('abort');
        xhr._fireEvent('loadend');
      } else {
        // _fireEvent already invokes the on* property — no direct call,
        // it used to fire onerror/onabort twice (#248)
        xhr._fireEvent('error');
        xhr._fireEvent('loadend');
      }
    });
  }

  abort() {
    this._aborted = true;
    if (this._timeoutTimer) { clearTimeout(this._timeoutTimer); this._timeoutTimer = null; }
    if (this.readyState > 0 && this.readyState < 4) {
      this._setReadyState(4);
      this._fireEvent('abort');
      this._fireEvent('loadend');
    }
    this.readyState = 0;
  }

  addEventListener(type, handler) {
    __listenerGate(handler);
    const L = __lmap(this);
    if (!L[type]) L[type] = [];
    L[type].push(handler);
  }

  removeEventListener(type, handler) {
    const L = __evtStore.get(this);
    if (L && L[type]) {
      L[type] = L[type].filter(h => h !== handler);
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
    const L = __evtStore.get(this);
    const handlers = (L && L[type]) || [];
    _mtWindow(() => { for (const h of handlers) { try { _invokeListener(h, this, ev); } catch (e) { globalThis.__diting_reportUncaughtError(e); } } });
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
    const L = __evtStore.get(this);
    const handlers = (L && L[type]) || [];
    _mtWindow(() => { for (const h of handlers) { try { _invokeListener(h, this, event); } catch(e) { globalThis.__diting_reportUncaughtError(e); } } });
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

