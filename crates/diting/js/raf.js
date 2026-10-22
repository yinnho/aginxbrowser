// requestAnimationFrame (split from bootstrap.js): callbacks run on a ~60Hz
// frame tick, every callback in one frame gets the same DOMHighResTimeStamp,
// and callbacks requested while a frame runs wait for the next one (the old
// setTimeout(fn, 0) shim spun hot and passed no timestamp, so libraries that
// do `t - start` got NaN). Call-time only.
(() => {
  let nextId = 0;
  let timer = 0;
  const pending = new Map();
  const frame = () => {
    timer = 0;
    const ts = performance.now();
    const batch = [...pending];
    pending.clear();
    for (const [, fn] of batch) {
      try { fn(ts); } catch (e) { globalThis.__diting_reportUncaughtError?.(e); }
    }
  };
  globalThis.__diting_raf = function (fn) {
    if (typeof fn !== 'function') {
      throw new TypeError("Failed to execute 'requestAnimationFrame' on 'Window': The callback provided as parameter 1 is not a function.");
    }
    const id = ++nextId;
    pending.set(id, fn);
    if (!timer) timer = setTimeout(frame, 16);
    return id;
  };
  globalThis.__diting_caf = function (id) { pending.delete(Number(id)); };
})();
