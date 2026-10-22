// Small Web API surfaces the Chrome diff battery found missing (split from
// bootstrap.js): scheduler.postTask/yield, Blob.stream, document.startView-
// Transition, the document.queryCommand* family, xmlns on XMLSerializer's
// HTML output, and a WAAPI animation object with a live playState plus getAnimations() registries.
// Everything is call-time and feature-checked so existing definitions win.
(() => {
  const def = (obj, name, value) => Object.defineProperty(obj, name, { value, writable: true, configurable: true });

  // ---- scheduler --------------------------------------------------------
  const PRIORITY_DELAY = { 'user-blocking': 0, 'user-visible': 1, background: 4 };
  const sched = globalThis.scheduler && typeof globalThis.scheduler === 'object' ? globalThis.scheduler : null;
  if (sched && typeof sched.postTask !== 'function') {
    def(Object.getPrototypeOf(sched) !== Object.prototype ? Object.getPrototypeOf(sched) : sched, 'postTask', function postTask(callback, options) {
      if (typeof callback !== 'function') return Promise.reject(new TypeError("Failed to execute 'postTask' on 'Scheduler': parameter 1 is not of type 'Function'."));
      const o = options || {};
      const signal = o.signal;
      if (signal && signal.aborted) return Promise.reject(signal.reason || new DOMException('The operation was aborted.', 'AbortError'));
      const delay = Math.max(0, Number(o.delay) || 0) + (PRIORITY_DELAY[o.priority] ?? 1);
      return new Promise((resolve, reject) => {
        let done = false;
        const t = setTimeout(() => {
          if (done) return;
          done = true;
          try { resolve(callback()); } catch (e) { reject(e); }
        }, delay);
        if (signal && typeof signal.addEventListener === 'function') {
          signal.addEventListener('abort', () => {
            if (done) return;
            done = true;
            clearTimeout(t);
            reject(signal.reason || new DOMException('The operation was aborted.', 'AbortError'));
          }, { once: true });
        }
      });
    });
    def(Object.getPrototypeOf(sched) !== Object.prototype ? Object.getPrototypeOf(sched) : sched, 'yield', function () {
      return new Promise((resolve) => setTimeout(resolve, 0));
    });
  }

  // ---- Blob.stream ------------------------------------------------------
  if (typeof Blob === 'function' && typeof Blob.prototype.stream !== 'function' && typeof ReadableStream === 'function') {
    def(Blob.prototype, 'stream', function stream() {
      const blob = this;
      return new ReadableStream({
        async start(controller) {
          const buf = await blob.arrayBuffer();
          if (buf.byteLength) controller.enqueue(new Uint8Array(buf));
          controller.close();
        },
      });
    });
    _markNative(Blob.prototype.stream);
  }

  // ---- View Transitions -------------------------------------------------
  if (typeof Document !== 'undefined' && typeof Document.prototype.startViewTransition !== 'function') {
    def(Document.prototype, 'startViewTransition', function startViewTransition(update) {
      const cb = typeof update === 'function' ? update : (update && update.update);
      const updateCallbackDone = new Promise((resolve, reject) => {
        Promise.resolve().then(() => (typeof cb === 'function' ? cb() : undefined)).then(resolve, reject);
      });
      const ready = updateCallbackDone.then(() => undefined);
      const finished = ready.then(() => undefined);
      ready.catch(() => {}); finished.catch(() => {}); updateCallbackDone.catch(() => {});
      return { updateCallbackDone, ready, finished, skipTransition() {}, types: new Set() };
    });
    _markNative(Document.prototype.startViewTransition);
  }

  // ---- document.queryCommand* ------------------------------------------
  const COMMANDS = new Set(['backcolor','bold','contentreadonly','copy','createlink','cut','decreasefontsize','defaultparagraphseparator','delete','fontname','fontsize','forecolor','formatblock','forwarddelete','hilitecolor','increasefontsize','indent','inserthorizontalrule','inserthtml','insertimage','insertlinebreak','insertorderedlist','insertparagraph','inserttext','insertunorderedlist','italic','justifycenter','justifyfull','justifyleft','justifyright','outdent','paste','redo','removeformat','selectall','strikethrough','styleWithCSS'.toLowerCase(),'subscript','superscript','underline','undo','unlink','usecss']);
  if (typeof Document !== 'undefined') {
    const name = (c) => String(c || '').toLowerCase();
    if (typeof Document.prototype.queryCommandSupported !== 'function') def(Document.prototype, 'queryCommandSupported', function queryCommandSupported(c) { return COMMANDS.has(name(c)); });
    if (typeof Document.prototype.queryCommandEnabled !== 'function') def(Document.prototype, 'queryCommandEnabled', function queryCommandEnabled(c) {
      const el = this.activeElement;
      const editable = !!el && (el.isContentEditable || (typeof _ns_isTextEntry === 'function' && _ns_isTextEntry(el)));
      return COMMANDS.has(name(c)) && (editable || ['copy', 'selectall'].includes(name(c)));
    });
    if (typeof Document.prototype.queryCommandState !== 'function') def(Document.prototype, 'queryCommandState', function queryCommandState() { return false; });
    if (typeof Document.prototype.queryCommandIndeterm !== 'function') def(Document.prototype, 'queryCommandIndeterm', function queryCommandIndeterm() { return false; });
    if (typeof Document.prototype.queryCommandValue !== 'function') def(Document.prototype, 'queryCommandValue', function queryCommandValue() { return ''; });
    for (const m of ['queryCommandSupported', 'queryCommandEnabled', 'queryCommandState', 'queryCommandIndeterm', 'queryCommandValue']) _markNative(Document.prototype[m]);
  }

  // ---- XMLSerializer: HTML elements carry the XHTML namespace ----------
  if (typeof XMLSerializer === 'function') {
    const orig = XMLSerializer.prototype.serializeToString;
    def(XMLSerializer.prototype, 'serializeToString', function serializeToString(node) {
      const s = orig.call(this, node);
      const el = node && node.nodeType === 9 ? node.documentElement : node;
      if (!el || el.nodeType !== 1 || el.namespaceURI !== 'http://www.w3.org/1999/xhtml' || el.hasAttribute('xmlns')) return s;
      return s.replace(/^(\s*(?:<!DOCTYPE[^>]*>)?\s*<[A-Za-z][^\s/>]*)/, '$1 xmlns="http://www.w3.org/1999/xhtml"');
    });
    _markNative(XMLSerializer.prototype.serializeToString);
  }

  // ---- WAAPI: a live animation object + getAnimations registries -------
  const live = new Set();
  const origAnimate = Element.prototype.animate;
  if (typeof origAnimate === 'function') {
    Element.prototype.animate = function animate(keyframes, options) {
      origAnimate.call(this, keyframes, options); // keeps the lifecycle events
      const o = typeof options === 'number' ? { duration: options } : (options || {});
      const duration = Math.max(0, Number(o.duration) || 0);
      const delay = Number(o.delay) || 0;
      const iterations = o.iterations === undefined ? 1 : Number(o.iterations);
      const endDelay = Number(o.endDelay) || 0;
      const active = iterations === Infinity ? Infinity : duration * iterations;
      const end = delay + active + endDelay;
      const target = this;
      let state = 'running', startedAt = performance.now(), held = 0, timer = 0;
      let resolveFinished, rejectFinished;
      const finished = new Promise((res, rej) => { resolveFinished = res; rejectFinished = rej; });
      finished.catch(() => {});
      const anim = {
        id: o.id || '',
        effect: { target, getTiming() { return { ...o, duration, delay, endDelay, iterations }; }, getComputedTiming() { return { duration, delay, endDelay, iterations, activeDuration: active, endTime: end }; }, getKeyframes() { return Array.isArray(keyframes) ? keyframes : []; } },
        timeline: document.timeline || null,
        playbackRate: 1,
        pending: false,
        onfinish: null, oncancel: null, onremove: null,
        get playState() { return state; },
        get currentTime() { return state === 'running' ? Math.min(end, held + (performance.now() - startedAt)) : (state === 'idle' ? null : held); },
        set currentTime(v) { held = Number(v) || 0; startedAt = performance.now(); },
        get startTime() { return state === 'idle' ? null : startedAt; },
        finished,
        ready: Promise.resolve(),
        _fire(type) {
          const ev = new Event(type);
          const h = this['on' + type];
          if (typeof h === 'function') { try { h.call(this, ev); } catch (e) {} }
          (this._l[type] || []).slice().forEach((f) => { try { f.call(this, ev); } catch (e) {} });
        },
        _l: {},
        addEventListener(t, f) { (this._l[t] = this._l[t] || []).push(f); },
        removeEventListener(t, f) { this._l[t] = (this._l[t] || []).filter((x) => x !== f); },
        _settle() {
          if (state !== 'running') return;
          state = 'finished'; held = end; live.delete(anim); clearTimeout(timer);
          resolveFinished(anim); anim._fire('finish');
        },
        play() { if (state === 'finished' || state === 'idle') { state = 'running'; held = 0; startedAt = performance.now(); live.add(anim); timer = setTimeout(() => anim._settle(), end); } else if (state === 'paused') { state = 'running'; startedAt = performance.now(); timer = setTimeout(() => anim._settle(), Math.max(0, end - held)); } },
        pause() { if (state === 'running') { held = anim.currentTime; state = 'paused'; clearTimeout(timer); } },
        finish() { if (state !== 'idle') { state = 'running'; anim._settle(); } },
        cancel() { if (state !== 'idle') { state = 'idle'; held = 0; live.delete(anim); clearTimeout(timer); rejectFinished(new DOMException('The user aborted a request.', 'AbortError')); anim._fire('cancel'); } },
        reverse() {}, updatePlaybackRate() {}, persist() {}, commitStyles() {},
      };
      live.add(anim);
      if (end !== Infinity) timer = setTimeout(() => anim._settle(), end);
      return anim;
    };
    _markNative(Element.prototype.animate);
    const forEl = (el, subtree) => [...live].filter((a) => a.effect.target === el || (subtree && el.contains && el.contains(a.effect.target)));
    def(Element.prototype, 'getAnimations', function getAnimations(opts) { return forEl(this, !!(opts && opts.subtree)); });
    _markNative(Element.prototype.getAnimations);
    if (typeof Document !== 'undefined' && typeof Document.prototype.getAnimations !== 'function') {
      def(Document.prototype, 'getAnimations', function getAnimations() { return [...live].filter((a) => a.effect.target.isConnected); });
      _markNative(Document.prototype.getAnimations);
    }
    if (typeof ShadowRoot !== 'undefined' && typeof ShadowRoot.prototype.getAnimations !== 'function') {
      def(ShadowRoot.prototype, 'getAnimations', function getAnimations() { return [...live].filter((a) => this.contains(a.effect.target)); });
      _markNative(ShadowRoot.prototype.getAnimations);
    }
  }
})();
