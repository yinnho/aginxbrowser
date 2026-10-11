// Async XHR delegates to the fetch shim below; this one-shot hint lets
// that single record carry initiatorType 'xmlhttprequest' instead of
// 'fetch' (consumed synchronously at shim entry, cleared after).
var _initiatorHint = null;


function _fetchInputUrl(input) {
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
  return url;
}
// Chrome parses the request URL at the API entry and throws TypeError
// SYNCHRONOUSLY (an async impl body can only reject the promise, which the
// page observably distinguishes). The op's SSRF gate fails closed on
// unparseable URLs too (#252) — this wrapper gives pages the Chrome face
// before any op involvement.
globalThis.fetch = function (input, init) {
  const _u = _fetchInputUrl(input);
  try { new URL(_u); } catch (e) { throw new TypeError("Failed to parse URL from " + _u); }
  return __ditingFetchImpl(input, init);
};
const __ditingFetchImpl = async (input, init = {}) => {
  // AbortSignal (fetch §4.1): a pre-aborted signal rejects before anything
  // else happens — no request, no scheme handling. Checked first so data:
  // fetches abort too (Chrome does the same: the signal gate precedes the
  // scheme fetch).
  const signal = init.signal !== undefined
    ? init.signal
    : (input instanceof Request ? input.signal : undefined);
  if (signal && signal.aborted) {
    throw signal.reason;
  }
  let url = _fetchInputUrl(input);
  // Fetch-spec "normalize a method" (obscura #969 family) — same rule as the
  // XHR open() path below: standard tokens get uppercased for the
  // case-sensitive CORS tables, custom tokens ride as authored.
  const rawMethod = init.method || (input instanceof Request ? input.method : "GET");
  const upMethod = String(rawMethod).toUpperCase();
  const method = /^(CONNECT|DELETE|GET|HEAD|OPTIONS|POST|PUT|TRACE)$/.test(upMethod) ? upMethod : String(rawMethod);
  // #126: bracket the WHOLE fetch — data:/blob: resolve locally below and
  // never reach the op call, but Chrome records their resource-timing entries
  // too (local decode sizes, zero wire cost). A delegated caller's hint
  // (async XHR) renames the initiator for this one record.
  const _rtT0 = globalThis.performance ? performance.now() : 0;
  const _rtInit = _initiatorHint || 'fetch';
  _initiatorHint = null;
  // data:/blob: never reach the HTTP client — it cannot fetch either scheme
  // (obscura #907 family). data: carries its MIME inline; blob: reads the
  // synchronous registry the real createObjectURL (in the Worker section
  // below) writes — the Blob itself with its _bytes, unlike the async
  // __blobStore text() copy which can lag a fetch issued right after
  // createObjectURL. Chrome rejects non-GET data: fetches.
  if (/^data:/i.test(url)) {
    if (method !== 'GET') {
      throw new TypeError("Fetching data: URLs with a method other than GET is unsupported");
    }
    // Chrome delivers even a local response on a task, never inline: resolving
    // here as a microtask lets `while (true) { await fetch('data:...') }` run
    // ~27k iterations/sec inside one microtask drain, starving every timer and
    // event on the page (measured: a 50ms timer dead for 3+ seconds). Same
    // shape as the XHR data:/blob: path's setTimeout(applyLocal, 0) hop.
    return await new Promise((resolve, reject) => {
      _scheduleAfter(0, 0, () => {
        let r;
        try {
          r = _dataUrlBytes(url);
        } catch (e) {
          __recordFetchTiming(_rtT0, url, _rtInit, null);
          reject(e);
          return;
        }
        __recordFetchTiming(_rtT0, url, _rtInit, { status: 200 }, {
          encodedBodySize: r.bytes.length, decodedBodySize: r.bytes.length, transferSize: 0,
        });
        resolve(new Response(r.bytes, {
          status: 200,
          statusText: "OK",
          headers: { 'content-type': r.mime },
          type: "basic",
          url,
          redirected: false,
        }));
      });
    });
  }
  if (/^blob:/i.test(url)) {
    const blob = globalThis.__blobObjs && globalThis.__blobObjs[url];
    return await new Promise((resolve, reject) => {
      _scheduleAfter(0, 0, () => {
        if (!blob || !(blob._bytes instanceof Uint8Array)) {
          __recordFetchTiming(_rtT0, url, _rtInit, null);
          reject(new TypeError('Failed to fetch: ' + url));
          return;
        }
        // Copy: Chrome snapshots blob contents at fetch time; sharing the stored
        // buffer would let one consumed Response's detach alias every later one.
        const bytes = new Uint8Array(blob._bytes);
        __recordFetchTiming(_rtT0, url, _rtInit, { status: 200 }, {
          encodedBodySize: bytes.length, decodedBodySize: bytes.length, transferSize: 0,
        });
        resolve(new Response(bytes, {
          status: 200,
          statusText: "OK",
          headers: blob.type ? { 'content-type': blob.type } : {},
          type: "basic",
          url,
          redirected: false,
        }));
      });
    });
  }
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
  // referrerPolicy/referrer ride down to the op so each redirect hop can
  // re-strip the Referer per the policy (obscura#875). Absent init falls
  // back to the Request object's value; a plain-URL fetch uses the document
  // with the default policy. '' referrer = explicitly no referrer.
  const fetchReferrerPolicy = init.referrerPolicy !== undefined
    ? String(init.referrerPolicy)
    : (input instanceof Request ? input.referrerPolicy : "");
  if (REFERRER_POLICIES.indexOf(fetchReferrerPolicy) < 0) {
    throw new TypeError("Failed to execute 'fetch': '" + fetchReferrerPolicy + "' is not a valid ReferrerPolicy value");
  }
  const fetchReferrer = init.referrer !== undefined
    ? String(init.referrer)
    : (input instanceof Request ? input.referrer : "about:client");
  const pageOrigin = (function() { try { const u = new URL(_domParse("document_url") || "about:blank"); return u.origin; } catch(e) { return ""; } })();
  // #226: key + outer promise so a settlement eaten by a mid-tick terminate
  // can still be recovered from the Rust-side mirror (see
  // __probeInflightFetches). Normal delivery resolves the outer promise via
  // the .then below; the probe is the second, idempotent path.
  const _fk = "fk" + (++__fetchKeySeq);
  const opCall = _OPS.op_fetch_url(url, method, hdrs, body, pageOrigin, fetchMode, fetchCredentials, fetchReferrerPolicy + '\u0000' + fetchReferrer + '\u0000' + _fk);
  let _resOuter, _rejOuter;
  const _outer = new Promise(function (res, rej) { _resOuter = res; _rejOuter = rej; });
  __inflightFetches.set(_fk, { resolveOuter: _resOuter, rejectOuter: _rejOuter });
  _fetchStage('armed', _fk);
  __armFetchProbeHeartbeat();
  opCall.then(
    function (raw) { _fetchStage('opok', _fk); __inflightFetches.delete(_fk); _resOuter(raw); },
    function (e) { _fetchStage('operr', _fk); __inflightFetches.delete(_fk); _rejOuter(e); }
  );
  // Mid-flight abort: race the op against the signal. There is no JS-to-Rust
  // cancellation channel, so the underlying walk keeps going and its eventual
  // settlement is swallowed — but the fetch() promise rejects at abort time
  // with the signal's reason, which is the observable contract pages race on.
  // #240 face-2 shape change: settle through a plain .then chain, not an
  // async-function resume. The 10-09 wedge lost a fetch AFTER its outer
  // promise resolved (in-flight key deleted) — the `await` continuation
  // simply never ran, zero terminates in the log. await-resume and .then
  // callbacks are two different V8 delivery paths for the same resolve;
  // the chain form drops async-resume machinery from this hot path (one
  // less suspect, one less frame) and the _fetchRing stages name
  // whichever path dies next.
  return (signal ? _raceAbortSignal(signal, _outer) : _outer).then(function (raw) {
    _fetchStage('resume', _fk);
    const parsed = JSON.parse(raw);
    __recordFetchTiming(_rtT0, url, _rtInit, parsed);
    if (parsed.blocked) {
      // #224 face: fetch() rejects rather than handing the page a fake
      // status-0 Response. No __aborted marker — that marker once made
      // XHR classify client-blocked requests as user aborts and fire
      // onabort instead of onerror (#248).
      const err = new TypeError('net::ERR_FAILED');
      err.name = 'AbortError';
      throw err;
    }
    if (parsed.corsBlocked) {
      throw new TypeError('Failed to fetch: ' + (parsed.corsError || 'CORS error'));
    }
    const respType = parsed.status === 0 ? "opaque" : (fetchMode === "no-cors" ? "opaque" : "basic");
    const responseBody = parsed.bodyBase64 ? _base64ToUint8Array(parsed.bodyBase64) : (parsed.body || "");
    _fetchStage('resp', _fk);
    return new Response(responseBody, {
      status: parsed.status,
      statusText: "",
      headers: parsed.headers || {},
      type: respType,
      // Chrome: Response.url is the FINAL URL after redirects and `redirected`
      // is true whenever one was followed. The op used to report only the
      // original URL, so a fetch() that mtop followed into a punish page read
      // as if it had landed on the API itself (the tmall report chased exactly
      // that ghost).
      url: parsed.final_url || parsed.url || url,
      redirected: !!parsed.redirected,
    });
  }, function (e) {
    _fetchStage('fthrow', _fk);
    __recordFetchTiming(_rtT0, url, _rtInit, null);
    throw e;
  });
};
// #235: XHR.send rides THIS closure, never the page-overridable
// globalThis.fetch. Pages that wrap fetch (doudian's SDK wraps XHR and fetch
// both) were re-processing already-signed XHRs through their own wrapper —
// captured as getSchema returning 200 with a 0-byte body. `_initiatorHint`
// above works unchanged: this is the same shim closure the XHR always
// intended to call. var (not const) matches the snapshot-era style around
// it and keeps a console/debugger `var` re-declaration benign.
var __ditingFetchShim = globalThis.fetch;
// fetch.js runs after bootstrap.js, so bootstrap's snapshot-time _markNative
// array (which listed globalThis.fetch when the shim lived mid-bootstrap)
// can no longer see it — mark here instead. _markNative is a bootstrap.js
// top-level function, shared via the same realm's global scope.
_markNative(globalThis.fetch);
