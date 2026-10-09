// MessagePort / MessageChannel — split out of bootstrap.js (god-file
// ratchet: the shared #213 channel + its #237 delivery-loss recovery). Runs
// AFTER bootstrap.js in the same realm: global lexical bindings from the first
// script (_runAsMacrotask, _markNative) are visible here, and nothing in
// bootstrap.js references these classes at definition time.

// MessagePort: a real two-end channel (#213). Spec shape — postMessage /
// onmessage / addEventListener('message') / start / close — with a per-port
// buffer that drains on start. Assigning onmessage starts the port, matching
// Chromium (a handler on the port IS the start signal), and so does adding a
// 'message' listener; postMessage before that queues instead of dropping, so
// a message sent while the far side is still booting is delivered, not lost.
// The delivery scheduler is per-port: MessageChannel below keeps the
// macrotask cadence React's scheduler depends on, while SharedWorker pairs
// default to a microtask — a signing channel sits on a booting SPA's critical
// path, where the main thread is busy for seconds and setTimeout delivery
// starves exactly while the SDK's own request timeout is counting down.
//
// #237: delivery is at-least-once. A macrotask-scheduled arm can die with a
// severed op chain (terminate_execution unwinds pending ops without settling
// them — the #226 family), and React 18's scheduler latches
// `isMessageLoopScheduled` on its FIRST post: one lost delivery permanently
// wedges every later async update on the page (doudian upload stuck at
// 上传中1%, message_port's own sweep used to only log). Each undelivered
// payload is tracked in _pending; the sweep and the termination-recovery hook
// re-arm stalled entries. A per-entry generation makes the superseded old arm
// a no-op, so re-arming can lose nothing but also never duplicates.
class MessagePort {
  constructor(deliver) {
    this._deliver = deliver || ((fn) => Promise.resolve().then(fn));
    this._onmessage = null;
    this._onmessageerror = null;
    this._other = null;
    this._started = false;
    this._closed = false;
    this._queue = [];
    this._listeners = [];
    // #237: payloads not yet delivered, one entry per post: {data, gen,
    // since}. `since` is the first-arm time — the wait window measures the
    // OLDEST undelivered payload, so it cannot drift (a lost arm can no
    // longer leak a counter decrement; the list itself is the truth).
    this._pending = [];
    __hideOwn(this);
  }
  get onmessage() { return this._onmessage; }
  set onmessage(fn) { this._onmessage = fn; if (typeof fn === 'function') this.start(); }
  get onmessageerror() { return this._onmessageerror; }
  set onmessageerror(fn) { this._onmessageerror = fn; }
  postMessage(data) {
    const other = this._other;
    if (!other || this._closed) return;
    other._enqueue(data);
  }
  _enqueue(data) {
    if (this._closed) return;
    if (!this._started) { this._queue.push(data); return; }
    this._dispatch(data);
  }
  _dispatch(data) {
    const entry = { data, gen: 0, since: 0 };
    this._pending.push(entry);
    this._armEntry(entry);
  }
  _armEntry(entry) {
    const myGen = ++entry.gen;
    if (!entry.since) entry.since = Date.now();
    const self = this;
    this._deliver(() => {
      // Superseded by a #237 re-arm: a fresher arm owns this payload, and
      // the entry stays in _pending for that arm.
      if (self._closed || myGen !== entry.gen) return;
      const idx = self._pending.indexOf(entry);
      if (idx !== -1) self._pending.splice(idx, 1);
      self._fire(entry.data);
    });
  }
  _fire(data) {
    const self = this;
    if (this._inflight) {
      console.error('[PORT-REENTRY] new delivery while one in flight (' +
        (Date.now() - this._inflightT) + 'ms); fn=' + String(this._onmessage).slice(0, 100));
    }
    this._inflight = true; this._inflightT = Date.now();
    try {
      const evt = { type: 'message', data, target: self, currentTarget: self };
      const h = self._onmessage;
      if (typeof h === 'function') {
        try { h.call(self, evt); } catch (e) { console.error('MessagePort onmessage error:', e && e.message, e && e.stack ? '\n' + e.stack : ''); }
      }
      for (const fn of self._listeners.slice()) {
        try { _invokeListener(fn, self, evt); } catch (e) { globalThis.__diting_reportUncaughtError(e); }
      }
    } finally {
      const dur = Date.now() - this._inflightT;
      this._inflight = false;
      if (dur > 2000) console.error('[PORT-SLOW] handler ran ' + dur + 'ms; fn=' + String(this._onmessage).slice(0, 100));
    }
  }
  // #237: re-arm every undelivered payload. The oldest entry's wait time
  // gates the sweep; termination recovery passes force=true because
  // in-flight arms died with the interrupted JS stack. A handler mid-run
  // will drain the rest itself — never re-enter it.
  _recoverStalled(force) {
    if (this._closed || !this._pending.length || this._inflight) return 0;
    if (!force && Date.now() - this._pending[0].since < _STALL_MS) return 0;
    const n = this._pending.length;
    for (const entry of this._pending.slice()) this._armEntry(entry);
    return n;
  }
  start() {
    if (this._started) return;
    this._started = true;
    const q = this._queue;
    this._queue = [];
    for (const d of q) this._dispatch(d);
  }
  addEventListener(type, fn) {
    if (type !== 'message') return;
    __listenerGate(fn);
    this._listeners.push(fn);
    this.start();
  }
  removeEventListener(type, fn) { if (type === 'message') this._listeners = this._listeners.filter(f => f !== fn); }
  close() { this._closed = true; this._queue = []; this._pending = []; }
}
Object.defineProperty(MessagePort.prototype, Symbol.toStringTag, { value: 'MessagePort', configurable: true });
function _newPortPair(deliver) {
  const a = new MessagePort(deliver);
  const b = new MessagePort(deliver);
  a._other = b;
  b._other = a;
  return [a, b];
}
class MessageChannel {
  constructor() {
    // Message delivery is a macrotask, exactly like a real browser: React's
    // Scheduler posts work through a MessageChannel port precisely because
    // delivery lands on a fresh task after the microtask queue drains.
    // Microtask delivery interleaves scheduler work with unrelated promise
    // chains and deterministically wedges transitions (observed as "server
    // actions dispatch from some realms and never from others").
    // Level 0: message delivery is a task, not a timer — it never joins a
    // setTimeout chain, so the 4ms nesting floor must not apply to it —
    // and per _runAsMacrotask it also refuses to fire inside another
    // task's synchronous window (the #327 guard above).
    const [p1, p2] = _newPortPair((fn) => _runAsMacrotask(fn));
    this.port1 = p1;
    this.port2 = p2;
  }
}
globalThis.MessageChannel = MessageChannel;
globalThis.MessagePort = MessagePort;
_markNative(MessagePort);
_markNative(MessageChannel);

// Delivery-loss recovery: a port whose armed delivery never runs (an
// op_sleep(0) chain that died with a terminate) wedges React's scheduler
// permanently — isMessageLoopScheduled stays true and no callback ever fires
// again. #237: the sweep now RE-ARMS stalled messages instead of only
// logging; a port stalled for minutes keeps re-arming every pass, which is
// the correct amount of noise for state the page cannot survive. Armed
// lazily on the first runtime port pair: bootstrap top level runs during
// snapshot creation, where no ops exist yet.
const _STALL_MS = 5000;
const _portReg = new Set();
let _sweepArmed = false;
function _portSweep() {
  for (const p of _portReg) {
    if (p._pending.length > 0 && Date.now() - p._pending[0].since > _STALL_MS) {
      const n = p._recoverStalled(false);
      if (n > 0)
        console.error('[PORT-RECOVER] re-armed ' + n + ' stalled message(s), waiting ' +
          (Date.now() - p._pending[0].since) + 'ms; fn=' + String(p._onmessage).slice(0, 120));
    }
    if (p._closed || (p._pending.length === 0 && !p._onmessage && p._listeners.length === 0)) _portReg.delete(p);
  }
}
function _armPortSweep() {
  if (_sweepArmed) return;
  _sweepArmed = true;
  setInterval(_portSweep, 5000);
}
// Test/debug probe (#237): run one sweep pass now. The interval is the
// production path; tests and live diagnosis call this directly.
globalThis.__ditingPortSweep = () => { _portSweep(); };
{
  const origPair = _newPortPair;
  _newPortPair = function (deliver) {
    const pair = origPair(deliver);
    _portReg.add(pair[0]); _portReg.add(pair[1]);
    _armPortSweep();
    return pair;
  };
}
// #237: terminate_execution severs in-flight op arms. The engine's
// termination recovery point is the one moment where nothing JS is running
// and any still-pending port arm is necessarily orphaned — re-arm every
// undelivered message there, not 5s later. Late-bound wrap: bootstrap.js
// owns the hook and runs before this file.
{
  const _origRecover = globalThis.__diting_mt_recover_termination;
  if (typeof _origRecover === 'function') {
    globalThis.__diting_mt_recover_termination = function () {
      try { for (const p of _portReg) p._recoverStalled(true); } catch (_) {}
      return _origRecover.apply(this, arguments);
    };
  }
}
