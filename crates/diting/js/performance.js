// (#98 extraction split) The Performance family, moved out of bootstrap.js
// (god-file ratchet: the file only shrinks). Runs after bootstrap.js in the
// same realm — __hideOwn/_markNativeProto/_markNative/__def/_fpRand/
// _domRaw/_OPS are bootstrap top-level bindings shared across scripts,
// touched only at call time, exactly as they were in-file.

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
    __hideOwn(this);
  }
  get name() { return this._peName; }
  get entryType() { return this._peType; }
  get startTime() { return this._peStart; }
  get duration() { return this._peDur; }
  // #192: Chrome carries detail on the prototype (null when never set).
  get detail() { return this._peDetail; }
  toJSON() {
    const o = { name: this._peName, entryType: this._peType,
                startTime: this._peStart, duration: this._peDur };
    if (this._peDetail !== null) o.detail = this._peDetail;
    return o;
  }
}
_markNativeProto(PerformanceEntry.prototype);
// Resource/ServerTiming entry classes: prototype accessors (same
// lie-detector posture as FontFace above). Resource entries carry REAL
// measurements since #126 — fetch/XHR samples captured in the shims
// below (resource clock directly) plus the navigation pipeline's
// stylesheet/script fetch timings handed over via the
// `resource_timings` dom command (epoch-stamped, converted here).
// Requests without a real measurement still get no entry — the buffer
// never fabricates.
const _RT_INTERNAL = Symbol();
globalThis.PerformanceEntry = PerformanceEntry;
globalThis.PerformanceResourceTiming = class PerformanceResourceTiming extends PerformanceEntry {
  constructor(rec, tok) {
    if (tok !== _RT_INTERNAL) throw new TypeError("Illegal constructor");
    super(rec.name, 'resource', rec.startTime, rec.duration);
    this._prt = rec;
    __hideOwn(this);
  }
  get initiatorType() { return this._prt.initiatorType || "other"; }
  get nextHopProtocol() { return ""; }
  get renderBlockingStatus() { return this._prt.renderBlocking ? "blocking" : "non-blocking"; }
  get deliveryType() { return ""; }
  get secureConnectionStart() { return 0; }
  get transferSize() { return this._prt.transferSize || 0; }
  get encodedBodySize() { return this._prt.encodedBodySize || 0; }
  get decodedBodySize() { return this._prt.decodedBodySize || 0; }
  get responseStatus() { return this._prt.responseStatus || 0; }
  toJSON() {
    return Object.assign(PerformanceEntry.prototype.toJSON.call(this), {
      initiatorType: this.initiatorType, nextHopProtocol: "",
      renderBlockingStatus: this.renderBlockingStatus, deliveryType: "",
      secureConnectionStart: 0, transferSize: this.transferSize,
      encodedBodySize: this.encodedBodySize, decodedBodySize: this.decodedBodySize,
      responseStatus: this.responseStatus,
    });
  }
};
_markNativeProto(globalThis.PerformanceResourceTiming.prototype);
// #192: the remaining four entry constructors Chrome exposes. Mark is
// page-constructible (startTime defaults 0 — construction never reads the
// clock); the other three mint only through the shared internal token and
// throw Illegal constructor from page code, the same posture as
// PerformanceResourceTiming above. github's analytics references
// PerformanceNavigationTiming as a bare global — the old "don't expose the
// constructor so detectors can't diff descriptors" stance turned that into a
// ReferenceError, which costs far more than a diffable prototype ever did.
globalThis.PerformanceMark = class PerformanceMark extends PerformanceEntry {
  constructor(name, options) {
    const o = options || {};
    const t = typeof o.startTime === 'number' && isFinite(o.startTime) ? o.startTime : 0;
    super(String(name), 'mark', t, 0);
    if ('detail' in o) this._peDetail = o.detail;
    __hideOwn(this);
  }
};
_markNativeProto(globalThis.PerformanceMark.prototype);
globalThis.PerformanceMeasure = class PerformanceMeasure extends PerformanceEntry {
  constructor(name, startTime, duration, tok) {
    if (tok !== _RT_INTERNAL) throw new TypeError("Illegal constructor");
    super(String(name), 'measure', startTime, duration);
  }
};
_markNativeProto(globalThis.PerformanceMeasure.prototype);
globalThis.PerformancePaintTiming = class PerformancePaintTiming extends PerformanceEntry {
  constructor(name, startTime, tok) {
    if (tok !== _RT_INTERNAL) throw new TypeError("Illegal constructor");
    super(String(name), 'paint', startTime, 0);
  }
};
_markNativeProto(globalThis.PerformancePaintTiming.prototype);
globalThis.PerformanceNavigationTiming = class PerformanceNavigationTiming extends PerformanceEntry {
  constructor(name, duration, extra, tok) {
    if (tok !== _RT_INTERNAL) throw new TypeError("Illegal constructor");
    super(String(name), 'navigation', 0, duration);
    // Level-2 fields stay own data props — the existing _navTiming posture.
    Object.assign(this, extra);
  }
};
_markNativeProto(globalThis.PerformanceNavigationTiming.prototype);
globalThis.PerformanceServerTiming = class PerformanceServerTiming extends PerformanceEntry {
  constructor() { throw new TypeError("Illegal constructor"); }
  get serverTimingName() { return this._peName; }
  get description() { return ""; }
};
_markNativeProto(globalThis.PerformanceServerTiming.prototype);
globalThis.EventCounts = class EventCounts {
  constructor() { throw new TypeError("Illegal constructor"); }
  get size() { return 0; }
  has() { return false; }
  get() { return 0; }
  forEach() {}
  entries() { return (function* () {})(); }
  keys() { return (function* () {})(); }
  values() { return (function* () {})(); }
};
_markNativeProto(globalThis.EventCounts.prototype);
class _Performance {
  constructor() {
    this._marks = []; this._measures = [];
    this._navEntry = null; this._paintEntries = null;
    this._resources = [];
    this._resourceCap = 250;
    this.timeOrigin = 0;
    this.timing = { navigationStart: 0, domContentLoadedEventEnd: 0, loadEventEnd: 0 };
    this.navigation = { type: 0, redirectCount: 0 };
    this.memory = {
      jsHeapSizeLimit: 2172649472,
      totalJSHeapSize: 19321856,
      usedJSHeapSize: 16781520,
    };
    __hideOwn(this);
  }
  now() {
    // (#78) Chrome reports sub-ms floats (100µs coarsening); an integral
    // Date.now() delta is a timing-entropy tell on the jsvmp scan path.
    // The monotonic contract in the comment above still holds — _perfLast
    // floors any regression.
    var ms = _OPS.op_clock_ms() - (this.timeOrigin || 0);
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
    // mark() stamps the clock when startTime is absent; the constructor
    // alone defaults it to 0 — Chrome keeps the two paths distinct too.
    const opts = (typeof o.startTime === 'number' && isFinite(o.startTime))
      ? o : Object.assign({}, o, { startTime: t });
    const e = new PerformanceMark(String(name), opts);
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
        const t0 = this._resolveTimeRef(start);
        if (t0 === null) throw new SyntaxError("Failed to execute 'measure' on 'Performance': The mark '" + start + "' does not exist.");
        startTime = t0;
      } else if (typeof start === 'number' && isFinite(start)) {
        startTime = start;
      }
    }
    if (end !== undefined && end !== null) {
      if (typeof end === 'string') {
        const t1 = this._resolveTimeRef(end);
        if (t1 === null) throw new SyntaxError("Failed to execute 'measure' on 'Performance': The mark '" + end + "' does not exist.");
        endTime = t1;
      } else if (typeof end === 'number' && isFinite(end)) {
        endTime = end;
      }
    }
    // Chrome allows a negative duration when end < start; keep that.
    const e = new PerformanceMeasure(String(name), startTime, endTime - startTime, _RT_INTERNAL);
    this._measures.push(e);
    if (this._measures.length > 500) this._measures.shift();
    return e;
  }
  // A start/end string names a user mark first, then falls back to the
  // User Timing L2 rule: PerformanceTiming attribute names resolve against
  // the timing object, relative to navigationStart. PDD's perf SDK measures
  // from 'navigationStart' on every page boot — Chrome always resolves that
  // to 0; throwing there poisoned the goods-list React mount (2026-10-12).
  _resolveTimeRef(name) {
    const m = this._lastMark(name);
    if (m) return m.startTime;
    const t = this.timing || {};
    if (Object.prototype.hasOwnProperty.call(t, name) && typeof t[name] === 'number') {
      const nav0 = t.navigationStart || this.timeOrigin;
      return Math.max(0, t[name] - nav0);
    }
    return null;
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
    const e = new globalThis.PerformanceNavigationTiming(
      (globalThis.location && globalThis.location.href) || '',
      Math.max(0, loadEnd - nav0), extra, _RT_INTERNAL);
    e.toJSON = function() {
      return Object.assign(PerformanceEntry.prototype.toJSON.call(this), extra);
    };
    _markNative(e.toJSON);
    __def(this, '_navEntry', e);
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
      new globalThis.PerformancePaintTiming('first-paint', fp, _RT_INTERNAL),
      new globalThis.PerformancePaintTiming('first-contentful-paint', fcp, _RT_INTERNAL),
    ];
    __hideOwn(this);
    return this._paintEntries;
  }
  _all() {
    const nav = this._navTiming();
    const list = (nav ? [nav] : []).concat(this._paintTimings(), this._resourcesAll(), this._marks, this._measures);
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
    if (type === 'resource') return this._resourcesAll().slice();
    // Unknown types return [] like Chrome.
    return [];
  }
  // ---- resource timing (#126) ----
  // Two faces, both real: shim-captured fetch/XHR samples (recorded here
  // in the resource clock directly) and the navigation pipeline's
  // stylesheet/script fetch timings (epoch-stamped in Rust, pulled on
  // demand and converted). Nothing is synthesized — a request without a
  // measurement stays absent, which is exactly the failure mode #126
  // filed against the old always-empty buffer.
  _recordResource(o) {
    if (!o || typeof o.name !== 'string') return;
    if (this._resources.length >= this._resourceCap) return;
    this._resources.push(new PerformanceResourceTiming(o, _RT_INTERNAL));
  }
  _rustResources() {
    let arr;
    try { arr = JSON.parse(_domRaw("resource_timings", "", "") || "[]"); }
    catch (e) { return []; }
    if (!Array.isArray(arr)) return [];
    const to = this.timeOrigin || 0;
    return arr.map((r) => ({
      name: String(r.name || ''),
      // Epoch → resource clock. Stylesheet/script fetches genuinely
      // start before __diting_init sets timeOrigin, so the clamp is the
      // honest floor for them; the duration is the real measurement.
      startTime: Math.max(0, (Number(r.start) || 0) - to),
      duration: Math.max(0, Number(r.duration) || 0),
      initiatorType: r.initiatorType || 'other',
      transferSize: r.bodySize || 0,
      encodedBodySize: r.bodySize || 0,
      decodedBodySize: r.bodySize || 0,
      responseStatus: r.status || 0,
      renderBlocking: !!r.renderBlocking,
    }));
  }
  _resourcesAll() {
    const rust = this._rustResources().map((o) => new PerformanceResourceTiming(o, _RT_INTERNAL));
    return this._resources.concat(rust).sort((a, b) => a.startTime - b.startTime);
  }
  clearMarks(name) {
    if (name === undefined) { this._marks = []; return; }
    this._marks = this._marks.filter((e) => e.name !== String(name));
  }
  clearMeasures(name) {
    if (name === undefined) { this._measures = []; return; }
    this._measures = this._measures.filter((e) => e.name !== String(name));
  }
  setResourceTimingBufferSize(maxSize) {
    const v = Number(maxSize);
    if (isFinite(v) && v >= 0) this._resourceCap = Math.floor(v);
  }
  clearResourceTimings() {
    this._resources = [];
    try { _domRaw("resource_timings_clear", "", ""); } catch (e) {}
  }
  get eventCounts() {
    if (!this._evtCounts) { this._evtCounts = Object.create(EventCounts.prototype); }
    return this._evtCounts;
  }
  // Per-navigation cache reset, called from __diting_init after timing lands.
  _resetDerived() { this._navEntry = null; this._paintEntries = null; }
}
_markNativeProto(_Performance.prototype);
globalThis.performance = globalThis.performance || new _Performance();
