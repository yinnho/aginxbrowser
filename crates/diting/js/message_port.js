// MessagePort / MessageChannel — split out of bootstrap.js (god-file
// ratchet: the shared #213 channel + its delivery-loss sweep). Runs AFTER
// bootstrap.js in the same realm: global lexical bindings from the first
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
    const self = this;
    this._pendCount = (this._pendCount || 0) + 1;
    if (this._pendCount === 1) this._pendSince = Date.now();
    this._deliver(() => {
      this._pendCount--;
      if (this._closed) return;
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
    });
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
  close() { this._closed = true; this._queue = []; }
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

// Delivery-loss detector: a port whose armed delivery never runs (e.g. an
// op_sleep(0) chain that died) wedges React's scheduler permanently —
// isMessageLoopScheduled stays true and no callback ever fires again.
// Armed lazily on the first runtime port pair: bootstrap top level runs
// during snapshot creation, where no ops exist yet.
const _portReg = new Set();
let _sweepArmed = false;
function _armPortSweep() {
  if (_sweepArmed) return;
  _sweepArmed = true;
  setInterval(() => {
    for (const p of _portReg) {
      if (p._pendCount > 0 && Date.now() - p._pendSince > 5000) {
        console.error('[PORT-LOST] ' + p._pendCount + ' undelivered message(s) for ' +
          (Date.now() - p._pendSince) + 'ms; fn=' + String(p._onmessage).slice(0, 120));
        p._pendSince = Date.now();
      }
      if (p._closed || ((p._pendCount || 0) === 0 && !p._onmessage && p._listeners.length === 0)) _portReg.delete(p);
    }
  }, 5000);
}
{
  const origPair = _newPortPair;
  _newPortPair = function (deliver) {
    const pair = origPair(deliver);
    _portReg.add(pair[0]); _portReg.add(pair[1]);
    _armPortSweep();
    return pair;
  };
}
