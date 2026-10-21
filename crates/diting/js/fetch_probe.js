// #226 settlement-recovery registry for the fetch shim (split from
// bootstrap.js): the in-flight key table, the probe that drains the
// Rust-side mirror into eaten promises, and the lazy heartbeat. Load order
// keeps this after bootstrap.js; every reference is call-time, not load-time.
// #226: settlement recovery for fetches eaten by a mid-tick watchdog
// terminate. deno_core pops completed op results inside
// dispatch_event_loop_tick; if the terminate lands before the await
// continuation runs, the fetch hangs forever with the server already
// responded (phase dispatchers swallow the termination, so there is no
// error anywhere). The Rust side mirrors every network fetch completion
// into a bounded table keyed here; the probe below takes matching
// entries and settles the outer promise manually. Runs from the
// macrotask-recover hook (every unwind) and a slow heartbeat; probing an
// empty registry is one length check.
var __fetchKeySeq = 0;
var __inflightFetches = new Map();
var __fetchProbeTimer = 0;
function __probeInflightFetches() {
  if (!__inflightFetches.size) return;
  var keys = Array.from(__inflightFetches.keys());
  for (var i = 0; i < keys.length; i++) {
    var raw;
    try { raw = _OPS.op_fetch_take(keys[i]); } catch (e) { continue; }
    if (raw === "") continue;
    var ent = __inflightFetches.get(keys[i]);
    __inflightFetches.delete(keys[i]);
    if (raw.charCodeAt(0) === 0) {
      _fetchStage('takerej', keys[i]);
      ent.rejectOuter(new TypeError(raw.slice(1) || "fetch settlement lost"));
    } else {
      _fetchStage('takeok', keys[i]);
      ent.resolveOuter(raw);
    }
  }
}
// Lazy heartbeat (created with the first in-flight fetch, never during
// snapshot creation — a boot-time interval would keep the snapshot build's
// event-loop drain from ever going idle): covers fire-less losses, where a
// settlement was eaten with no watchdog unwind to trigger the recover hook.
function __armFetchProbeHeartbeat() {
  if (!__fetchProbeTimer) __fetchProbeTimer = setInterval(function () { __probeInflightFetches(); }, 5000);
}

// Settle early-exit probe: true when nothing the page owns can still fire
// inside the next `win` ms — no in-flight fetch, no queued or running
// macrotask, and the earliest timer lands after the window. Lets
// Page::settle_pending_work stop at the first quiet moment instead of
// idling out its whole fast-path budget (the event loop itself never reads
// idle while any far-off timer is armed).
globalThis.__diting_settle_quiet = function (win) {
  if (__inflightFetches.size) return false;
  var s = globalThis.__MT_STATE();
  if (s.depth || s.armed || s.qlen) return false;
  var w = s.wheel;
  return !w.heap || w.armedIn === null || w.armedIn > win;
};
