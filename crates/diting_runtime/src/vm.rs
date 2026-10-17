//! The V8 vertical slice — a bare isolate driven by our own loop.
//!
//! Batch 2 proved the seam end to end on raw `v8`, with no deno_core
//! anywhere: one isolate, one context, one JS settlement face, and a loop
//! pass that runs the [`SettleQueue`] into JS and drains microtasks. The
//! acceptance pin is the campaign's minimal closed loop: terminate lands
//! mid-settlement → the batch parks instead of evaporating →
//! `cancel_terminate_execution` revives the isolate → the next poll
//! re-delivers → the awaiting promise settles. The #226/#240 shape, owned.
//!
//! What this slice deliberately does NOT have yet: the immediates/close
//! phases (diting uses neither — no setImmediate, no close callbacks
//! anywhere in the tree). The op driver (batch 3), the ring promise table
//! (batch 4, replacing the original Map seed — same settle law,
//! deno_core's ring shape learned and one deliberate divergence:
//! Missing-absorb instead of deno's throw), and the snapshot face
//! (batch 5: [`create_snapshot`] / [`Vm::from_snapshot`] — the dormant
//! shell baked once, restored per instance) are wired in. See
//! [`crate::driver`], [`crate::snapshot`], and the PRELUDE below.

use crate::driver::{AfterTicks, OpDriver, WakeFlag};
use crate::modules::dynamic;
use crate::modules::eval::ModuleEvalError;
use crate::modules::load::RecursiveLoad;
use crate::modules::map::{ModuleMap, ModuleSlot, initialize_import_meta_object_callback};
use crate::modules::{ModuleError, ModuleId, ModuleLoader};
use crate::settle::SettleQueue;
use crate::snapshot::{self, SnapshotError};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::sync::Once;
use std::task::{Context, Poll};

/// Triplets per delivery batch. deno_core ships the whole completed-ops vec
/// in one `__eventLoopTick` call; we settle per-triplet (the JS claim layer
/// in js/core_base.js already proved per-triplet calls are noise: ≤341 in
/// the worst real trace, against a cross-FFI boundary the settle ring pays
/// anyway). 512 keeps batches bounded without a second trip through the
/// queue.
const BATCH_CAP: usize = 512;

/// The JS face of settlement — our own promise table, ring-shaped.
///
/// The law is deno_core's (00_infra.js, learned not copied): a fixed ring
/// holds the live promises, `slot = id % RING_SIZE`; a slot reclaimed
/// while its entry is still pending evicts that entry to an overflow Map
/// keyed on the entry's OWN id (so the id counter wrapping back to zero
/// can never mistake a reused slot for the older promise). Ids stay
/// non-negative i32 — the counter wraps at 2^31-1, same arithmetic deno's
/// async op stubs use. Known inherited limit (documented upstream too): a
/// promise pending while 2^31 more ids dispatch gets its id reused and
/// the older map entry silently lost — no real workload spans that.
///
/// ONE deliberate divergence: deno's resolve THROWS on a missing id
/// ("Missing promise @ N"). Ours absorbs — a settle racing a page
/// teardown or a parked-batch replay must not kill the batch (a throw
/// parks + poison-drops it), and the Missing-absorb law is already
/// pinned by the vm tests. Kept in sync with the Rust call in
/// [`deliver_batch`]: (id, ok, payload).
const PRELUDE: &str = r#"
"use strict";
const RING_SIZE = 4 * 1024;
let nextPromiseId = 0;
// slot = [resolve, reject, id] or null; the id rides along so a reused
// slot is never mistaken for the evicted promise now living in the map.
const promiseRing = new Array(RING_SIZE).fill(null);
const promiseMap = new Map();

function __diting_register() {
  const id = nextPromiseId;
  nextPromiseId = (id + 1) & 0x7fffffff;
  const idx = id % RING_SIZE;
  const old = promiseRing[idx];
  if (old !== null) promiseMap.set(old[2], old);
  const promise = new Promise(function (resolve, reject) {
    promiseRing[idx] = [resolve, reject, id];
  });
  return { id: id, promise: promise };
}

globalThis.__diting_lastResolved = null;
globalThis.__diting_lastRejected = null;
globalThis.__diting_settle = function (id, ok, payload) {
  const idx = id % RING_SIZE;
  let entry = promiseRing[idx];
  if (entry !== null && entry[2] === id) {
    promiseRing[idx] = null;
  } else {
    entry = promiseMap.get(id);
    if (entry === undefined) return; // stale or unknown: absorb, keep delivering
    promiseMap.delete(id);
  }
  if (ok) entry[0](payload);
  else entry[1](new Error(payload));
};
// Driver probe: register a promise in the table, return its id. The .then
// taps make settlement observable from Rust without extra plumbing — the
// microtask drain flips the global, eval reads it.
globalThis.__diting_defer = function () {
  const r = __diting_register();
  r.promise.then(
    function (v) { globalThis.__diting_lastResolved = String(v); },
    function (e) { globalThis.__diting_lastRejected = String(e && e.message || e); }
  );
  return r.id;
};
// The async op face: register a promise, hand the op to Rust
// (__diting_spawn is a raw native callback installed from the Rust side),
// return the promise. This is the op_sleep shape bootstrap.js runs page
// timers on; here it drives AfterTicks futures inside the driver.
globalThis.__diting_op_sleep = function (ticks, payload) {
  const r = __diting_register();
  globalThis.__diting_spawn(r.id, ticks, String(payload));
  return r.promise;
};
// The async-op stub (batch 10c): install wraps every async op's raw fn
// in this trampoline — allocate the ring-table promise, prepend its id,
// hand off to Rust, return the promise. deno_core's setUpAsyncStub law:
// async wrappers never build promises on the Rust side, so no op callback
// ever needs to call JS.
globalThis.__diting_wrap_async = function (raw) {
  return function () {
    const r = __diting_register();
    const withId = [r.id];
    for (let i = 0; i < arguments.length; i++) withId.push(arguments[i]);
    raw.apply(null, withId);
    return r.promise;
  };
};
// Rejection-handler registration (10d): the raw tracking lives in Rust
// (promise_reject_callback + the RejectionSlot); the HANDLERS live in JS land
// so a snapshot restore needs no handle rehydration — deno_core 0.411
// serialized the registered Global into its snapshot sidecar instead
// (runtime/snapshot.rs), the one face our container law rejects: bake the
// binding, not the handle. The host's shell exposes these as
// Deno.core.set{Unhandled,Handled}PromiseRejectionHandler while the shell
// lives; the host's bootstrap registers its window-event handlers at bake
// time and the closures ride the blob. Rust reads the handler globals live
// each dispatch round (Vm::dispatch_rejections).
globalThis.__diting_set_unhandled_rejection_handler = function (h) {
  globalThis.__ditingUnhandledRejectionHandler = h;
};
globalThis.__diting_set_handled_rejection_handler = function (h) {
  globalThis.__ditingHandledRejectionHandler = h;
};
// Test taps — deno exposes the same pair for wraparound testing.
globalThis.__diting_setNextId = function (n) { nextPromiseId = n; };
globalThis.__diting_tableCounts = function () {
  let ring = 0;
  for (let i = 0; i < RING_SIZE; i++) if (promiseRing[i] !== null) ring++;
  return ring + "," + promiseMap.size;
};
"#;

/// V8 initializes once per process (platform + engine). Same pattern as
/// deno_core's `init_v8`, minus its flag dance: our platform, non-
/// snapshotting. This crate's tests run in their own binary; when the crate
/// eventually replaces deno_core inside the diting runtime, this is the
/// only init left standing.
fn ensure_v8() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        v8::V8::initialize_platform(crate::foreground::platform());
        v8::V8::initialize();
    });
}

/// One completed async-op result on its way to a JS promise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Triplet {
    pub promise_id: i32,
    pub ok: bool,
    /// Serialized payload. Resolve hands it to JS as a string; reject wraps
    /// it in an Error. Richer value shapes arrive with the real op faces.
    pub payload: String,
}

/// Why a delivery round failed. Both shapes park the batch; the difference
/// is who unblocks it: `Terminated` is the watchdog shape (revive, then
/// re-drive), `TickThrew` just needs the bug fixed — the message rides
/// along so the poison-drop path can surface it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopError {
    /// The termination interrupt bit this call. V8's terminate request is
    /// one-shot (pinned in the tests), so a next call may succeed without
    /// revive — the loop must not rely on either shape: park, re-drive,
    /// poison-drop after three failures.
    Terminated,
    /// The settlement call itself threw (not a termination interrupt).
    TickThrew(String),
}

/// Host-facing rendering (10d): diting formats entry-face errors with
/// `{}` and string-matches the termination symptom — "Uncaught Error:
/// execution terminated" is the exact shape the old deno_core host
/// surfaced, kept byte-for-byte so the host's
/// `contains("execution terminated")` heals stay honest.
impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopError::Terminated => f.write_str("Uncaught Error: execution terminated"),
            LoopError::TickThrew(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for LoopError {}

/// What one [`Vm::poll_event_loop`] pass found — the reactor host's idle
/// signal. `idle()` is deno's run_event_loop-resolution law in our shape:
/// nothing in flight means the loop's work is done (timers are op_sleep
/// promises here, so `pending_ops` IS the timer face).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopActivity {
    /// Settlements reached JS this pass.
    pub delivered: bool,
    /// Async ops without a completion yet.
    pub pending_ops: usize,
    /// A module evaluation or dynamic import is still in flight.
    pub pending_modules: bool,
    /// Promise-rejection events are still waiting to fire (deno's
    /// `has_pending_promise_events`: unhandled or handled pairs queued).
    /// The loop is not done while an event has not reached the page.
    pub pending_rejections: bool,
    /// V8 engine-side asynchronous work (wasm async compilation's
    /// background half) or queued foreground tasks — deno's
    /// `has_pending_background_tasks` law: the loop is not idle while the
    /// engine still owes the page a resolution.
    pub pending_background: bool,
}

impl LoopActivity {
    pub fn idle(&self) -> bool {
        self.pending_ops == 0
            && !self.pending_modules
            && !self.pending_rejections
            && !self.pending_background
    }
}

/// The driver lives in an isolate slot behind Rc<RefCell>: raw V8 function
/// callbacks have no closure capture, so `__diting_spawn` recovers the
/// isolate from the callback parts and finds the driver here (deno's ops
/// reach their OpCtx external the same way). RefCell because spawn can
/// fire from inside JS microtasks while the loop holds no borrow — the
/// loop drops its borrow before any JS runs (pinned by a test).
type DriverSlot = Rc<RefCell<OpDriver>>;

/// Promise-rejection tracking, the deno_core `exception_state.rs` law in our
/// shape: the raw V8 promise-reject callback appends (promise, reason) pairs
/// here; [`Vm::poll`] fires the JS handlers after the microtask drain.
/// Single-threaded next to the isolate like the DriverSlot.
#[derive(Default)]
struct RejectionState {
    /// Rejected with no handler, oldest first — unhandledrejection events
    /// waiting to fire.
    pending: VecDeque<(v8::Global<v8::Promise>, v8::Global<v8::Value>)>,
    /// Rejected, dispatched as unhandled, THEN handled late —
    /// rejectionhandled events, oldest first.
    pending_handled: VecDeque<(v8::Global<v8::Promise>, v8::Global<v8::Value>)>,
}

type RejectionSlot = Rc<RefCell<RejectionState>>;

/// The raw V8 promise-reject callback (installed in [`Vm::build`]).
///
/// Law verbatim from deno_core 0.411 `exception_state.rs::
/// track_promise_rejection` (MIT, 2018-2026 Deno authors):
/// `PromiseRejectWithNoHandler` → queue the (promise, reason) pair;
/// `PromiseHandlerAddedAfterReject` → retain-remove from the pending queue,
/// and when it was ALREADY dispatched (not found), queue a rejectionhandled
/// pair using the promise's own result as the reason. The two AfterResolved
/// events never warn (deno #1272). One divergence: deno gates the
/// rejectionhandled enqueue on a registered handled-callback (an enqueue-time
/// scope dance); we queue unconditionally and no-op at dispatch when nobody
/// listens — same observable events.
///
/// SAFETY: V8 invokes this on the isolate's thread under its callback
/// contract; the scope macro opens the handle scope Global creation needs.
/// No panics across the boundary — a missing slot is a silent no-op (the
/// promise would simply never fire an event, loud in every rejection test).
pub(crate) unsafe extern "C" fn promise_reject_callback(message: v8::PromiseRejectMessage) {
    use v8::PromiseRejectEvent::*;
    // SAFETY: `message` is alive for this callback; the macro enters the
    // scope V8 requires.
    v8::callback_scope!(unsafe scope, &message);
    let Some(slot) = scope.get_slot::<RejectionSlot>() else {
        return;
    };
    let promise = message.get_promise();
    match message.get_event() {
        PromiseRejectWithNoHandler => {
            let error = message.get_value().unwrap_or_else(|| {
                // deno unwrap()s here; the Option is real API surface and a
                // missing value pairs with undefined, not a panic in a raw
                // callback (a panic inside extern "C" is UB — abort).
                v8::undefined(scope).into()
            });
            let promise_global = v8::Global::new(scope, promise);
            let error_global = v8::Global::new(scope, error);
            slot.borrow_mut().pending.push_back((promise_global, error_global));
        }
        PromiseHandlerAddedAfterReject => {
            // The code has until the loop yields to attach a handler and
            // avoid the unhandled event. If we already delivered it, this is
            // a rejectionhandled pair instead.
            let promise_global = v8::Global::new(scope, promise);
            let mut st = slot.borrow_mut();
            let previous_len = st.pending.len();
            st.pending.retain(|(key, _)| key != &promise_global);
            if st.pending.len() == previous_len {
                // Drop the borrow before touching V8 again (promise.result).
                drop(st);
                let result = promise.result(scope);
                let result_global = v8::Global::new(scope, result);
                slot.borrow_mut()
                    .pending_handled
                    .push_back((promise_global, result_global));
            }
        }
        PromiseRejectAfterResolved | PromiseResolveAfterResolved => {}
    }
}

/// `__diting_spawn(id, ticks, payload)` — the JS→Rust op entry.
///
/// `pub(crate)` because [`crate::snapshot::EXTERNAL_REFERENCES`] pins this
/// exact function pointer into every blob; the two are one contract.
///
/// SAFETY: V8 invokes this under the API-callback contract (valid
/// FunctionCallbackInfo pointer, an open handle scope, on the isolate's
/// thread). We only touch the isolate through the recovered reference and
/// never outlive the call. No panics across this boundary — a panic inside
/// `extern "C"` is UB (abort), so a missing slot is a silent no-op: the
/// promise would hang, and every driver test would fail loudly anyway.
pub(crate) unsafe extern "C" fn spawn_callback(info: *const v8::FunctionCallbackInfo) {
    let info = unsafe { &*info };
    let parts = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    let int_arg = |i: i32| {
        args.get(i)
            .try_cast::<v8::Integer>()
            .map(|v| v.value())
            .unwrap_or(0)
    };
    let id = int_arg(0);
    let ticks = int_arg(1).clamp(0, u32::MAX as i64) as u32;
    let payload = args
        .get(2)
        .try_cast::<v8::String>()
        .map(|s| s.to_rust_string_lossy(isolate))
        .unwrap_or_default();
    if let Some(driver) = isolate.get_slot::<DriverSlot>() {
        driver.borrow_mut().spawn(AfterTicks {
            ticks,
            triplet: Triplet {
                promise_id: id as i32,
                ok: true,
                payload,
            },
        });
    }
}

/// The normalized completion of an async op body: `Ok` resolves the
/// promise with the payload string, `Err` rejects it with the message.
/// The `#[string]` / `JsErrorBox` return faces of the async four collapse
/// to this at their hand-written glue.
pub type AsyncOpOutcome = Result<String, String>;

/// The shared body of a hand-written async op (campaign batch 10c):
/// park the body future on the isolate's op driver against a promise id
/// the JS async stub already allocated in the ring table. The stub
/// (installed by [`crate::op_reg::OpSet::install_onto`] over the raw
/// wrapper — deno_core's `setUpAsyncStub` law, confirmed by
/// cargo-expanding its slow stub) allocates the promise, prepends the
/// id, and returns the promise; the raw wrapper decodes
/// `(promise_id, user args...)` and lands here. No V8 scope is needed:
/// the promise never gets built on the Rust side, settlement rides the
/// pipeline every driver completion rides — [`Vm::poll`] drives →
/// completion triplet → `__diting_settle` → the ring entry resolves or
/// rejects.
///
/// Returns false when the isolate has no driver slot (not our shell);
/// the raw wrapper then just returns — the stub's promise would hang,
/// loud in every test that exercises an async op.
pub fn spawn_async_op(
    isolate: &v8::Isolate,
    promise_id: i32,
    fut: impl Future<Output = AsyncOpOutcome> + 'static,
) -> bool {
    let Some(driver) = isolate.get_slot::<DriverSlot>() else {
        return false;
    };
    driver.borrow_mut().spawn(async move {
        match fut.await {
            Ok(payload) => Triplet {
                promise_id,
                ok: true,
                payload,
            },
            Err(message) => Triplet {
                promise_id,
                ok: false,
                payload: message,
            },
        }
    });
    true
}

/// Bake the JS face into a context: run the PRELUDE, install the raw
/// `__diting_spawn` callback. Both the fresh-vm path ([`Vm::build`]) and
/// the snapshot creator ([`create_snapshot`]) go through this one
/// function — the baked shell and the live one can never drift apart.
fn install_faces(isolate: &mut v8::OwnedIsolate, context: &v8::Global<v8::Context>) {
    v8::scope_with_context!(cs, isolate, context);
    let src = v8::String::new(cs, PRELUDE).expect("prelude source fits");
    let script = v8::Script::compile(cs, src, None).expect("prelude compiles");
    script.run(cs).expect("prelude runs");

    // The native op entry: a raw callback function, no template
    // machinery needed.
    let global = cs.get_current_context().global(cs);
    let spawn_name = v8::String::new(cs, "__diting_spawn").unwrap();
    let spawn_fn =
        v8::Function::new_raw(cs, spawn_callback).expect("__diting_spawn template builds");
    spawn_fn.set_name(spawn_name);
    global.set(cs, spawn_name.into(), spawn_fn.into());
}

/// Bake the dormant shell into our container format (batch 5). The
/// creator runs the same [`install_faces`] a fresh [`Vm`] runs, so the
/// blob IS a fresh vm's heap: promise table empty, faces installed, the
/// spawn callback pinned through [`crate::snapshot::external_references`]
/// — V8 stores snapshotted raw callbacks by INDEX into that table, so
/// every restore must pass the SAME table (append-only, order
/// load-bearing).
///
/// Snapshotted raw functions are exactly why the exclusivity discipline
/// exists: creation is a whole-isolate operation (the v8 crate runs its
/// own snapshot tests under a process-wide write lock). Within this
/// crate, that means snapshot tests live in their own test binary — the
/// creator here and a live isolate elsewhere in the same process at the
/// same moment is not a supported mix.
///
/// Panics (never returns None silently): a failed bake is a build bug,
/// and half a snapshot is worse than none.
pub fn create_snapshot(scripts: &[(&str, &str)]) -> Vec<u8> {
    ensure_v8();
    let mut isolate = v8::Isolate::snapshot_creator(Some(snapshot::external_references()), None);
    // Face-identical wiring: the prelude never CALLS the op face at bake
    // time, but the driver slot goes in anyway — if that ever changes,
    // tests get a loud hang instead of the callback's silent no-op drop.
    isolate.set_slot::<DriverSlot>(Rc::new(RefCell::new(OpDriver::new())));
    // Same posture for rejection tracking: bake scripts are quiet, but the
    // callback must never fire into a slotless isolate.
    isolate.set_slot::<RejectionSlot>(Rc::new(RefCell::new(RejectionState::default())));
    isolate.set_promise_reject_callback(promise_reject_callback);

    let context = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };
    install_faces(&mut isolate, &context);
    // Embedder scripts bake AFTER the faces — they see the prelude's
    // bindings at compile time, and everything they declare (const/let
    // lexical state included) lands in the blob. The tuple name rides only
    // the panic message; the compiled script itself carries no origin, so
    // baked frames surface as "<anonymous>" in Error.stack — diting's
    // brand-leak law (300015 env probes); embedders with their own law
    // pass their own list shape through unchanged.
    {
        v8::scope_with_context!(cs, &mut isolate, &context);
        v8::tc_scope!(let tc, cs);
        for (name, src) in scripts {
            let s = v8::String::new(tc, src).expect("script source fits");
            match v8::Script::compile(tc, s, None).and_then(|script| script.run(tc)) {
                Some(_) => {}
                None if tc.has_terminated() => {
                    panic!("{name} terminated during snapshot creation")
                }
                None => {
                    let msg = tc
                        .message()
                        .map(|m| m.get(tc).to_rust_string_lossy(tc))
                        .unwrap_or_else(|| "<no message>".into());
                    panic!("{name} failed during snapshot creation: {msg}");
                }
            }
        }
    }
    // The context we just dressed becomes THE context a restore revives:
    // set_default_context marks it as the blob's default context, and
    // Context::new on a snapshot-backed isolate returns exactly that.
    {
        v8::scope_with_context!(cs, &mut isolate, &context);
        let local = cs.get_current_context();
        cs.set_default_context(local);
    }
    // The Global must not survive into the bake: the serializer walks the
    // isolate's global-handle table and fatals on any handle it cannot
    // serialize (CheckGlobalAndEternalHandles — the v8 crate's own test
    // never creates one). Dropping it disposes the persistent handle; the
    // context itself lives on through set_default_context's bookkeeping.
    drop(context);

    // create_blob consumes the isolate and must not run inside any handle
    // scope — both scopes above are closed here.
    let startup_data = isolate
        .create_blob(v8::FunctionCodeHandling::Keep)
        .expect("snapshot blob builds");
    snapshot::serialize(&startup_data, &snapshot::sidecar_v1())
}

/// A bare V8 isolate driven by our loop. Single-threaded by design, like
/// everything that lives next to an isolate.
pub struct Vm {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    settle_fn: v8::Global<v8::Function>,
    queue: SettleQueue<Triplet>,
    /// Installed ops, kept for the isolate's lifetime — the Externals in
    /// the installed functions point into these boxes (op_reg's law).
    ops: Option<crate::op_reg::OpSet>,
    /// The poll-driven default waker for module-load futures: a flag the
    /// host can read advisory ("a module future woke"). A reactor-driven
    /// host (10d) passes its real waker to [`Vm::poll_modules`] instead.
    module_wake: std::sync::Arc<WakeFlag>,
    /// The foreground-task registry entry (wasm async compilation and other
    /// engine-side completions land here from any thread; the loop drains
    /// at pass top — [`crate::foreground`]).
    fg: std::sync::Arc<crate::foreground::ForegroundEntry>,
    /// The registry key (the raw isolate pointer). Drop deregisters — the
    /// key is only unique while the isolate lives.
    fg_key: usize,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

/// Deregister from the foreground-task registry BEFORE the isolate drops —
/// a later isolate can reuse the address, and a stale entry would route its
/// posts into a dead queue.
impl Drop for Vm {
    fn drop(&mut self) {
        crate::foreground::deregister(self.fg_key);
    }
}

impl Vm {
    pub fn new() -> Self {
        // A fresh isolate cannot fail to build — only the restore path has
        // error shapes, and those belong to from_snapshot.
        Self::build(None).expect("fresh isolate creation is infallible")
    }

    /// Revive a dormant shell baked by [`create_snapshot`]. The container
    /// is validated first ([`crate::snapshot::deconstruct`]); a v8 blob
    /// that is valid V8 but not OUR shell (no `__diting_settle` face)
    /// panics loudly — that is a wrong-blob bug, not a runtime condition.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, SnapshotError> {
        Self::build(Some(bytes))
    }

    fn build(snapshot: Option<&[u8]>) -> Result<Self, SnapshotError> {
        ensure_v8();
        let mut isolate = match snapshot {
            None => v8::Isolate::new(Default::default()),
            Some(bytes) => {
                let parts = snapshot::deconstruct(bytes)?;
                let params = v8::Isolate::create_params()
                    .snapshot_blob(v8::StartupData::from(parts.v8_blob.to_vec()))
                    .external_references(snapshot::external_references());
                v8::Isolate::new(params)
            }
        };
        // deno_core setup.rs `isolate_ptr_to_key` law: transmute the
        // repr(transparent) UnsafeRawIsolatePtr to the usize the platform's
        // post_task will hand back as the isolate key.
        let fg_key = {
            const _: () = assert!(
                std::mem::size_of::<v8::UnsafeRawIsolatePtr>()
                    == std::mem::size_of::<usize>()
            );
            // SAFETY: pointer-sized transparent wrapper, asserted above.
            unsafe { std::mem::transmute::<v8::UnsafeRawIsolatePtr, usize>(isolate.as_raw_isolate_ptr()) }
        };
        // Register BEFORE any other V8 call. Everything below — context
        // deserialization included — allocates, and allocating can trip GC
        // observers that post foreground tasks; an unregistered isolate's
        // posts are dropped, so a late registration is a silent-drop window
        // (deno #35408: registration must be unconditional from isolate
        // birth; a missing entry may only ever mean shutdown deregistered).
        let fg = crate::foreground::register(fg_key);
        // Microtasks run exactly when the loop drains them — never
        // implicitly at some V8-internal checkpoint. The loop owns the
        // schedule. (Runtime policy, not heap state — set on both paths.)
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        // The op driver goes in before any JS runs — __diting_spawn finds
        // it through this slot.
        isolate.set_slot::<DriverSlot>(Rc::new(RefCell::new(OpDriver::new())));
        // Rejection tracking from the first JS on: the raw callback finds
        // its queue through this slot (deno installs both at JsRuntime init).
        isolate.set_slot::<RejectionSlot>(Rc::new(RefCell::new(RejectionState::default())));
        isolate.set_promise_reject_callback(promise_reject_callback);

        let context = {
            v8::scope!(let scope, &mut isolate);
            // On a snapshot-backed isolate this returns the blob's default
            // context — the dressed shell. On a fresh isolate it is a new
            // context the prelude dresses below.
            let context = v8::Context::new(scope, Default::default());
            v8::Global::new(scope, context)
        };
        if snapshot.is_none() {
            // Restores already carry the faces in the blob.
            install_faces(&mut isolate, &context);
        }
        let settle_fn = {
            v8::scope_with_context!(cs, &mut isolate, &context);
            let name = v8::String::new(cs, "__diting_settle").unwrap();
            let settle_fn = cs
                .get_current_context()
                .global(cs)
                .get(cs, name.into())
                .and_then(|f| f.try_cast::<v8::Function>().ok())
                .expect("context has __diting_settle — without it this is not our shell");
            v8::Global::new(cs, settle_fn)
        };
        Ok(Self {
            isolate,
            context,
            settle_fn,
            queue: SettleQueue::new(),
            ops: None,
            module_wake: std::sync::Arc::new(WakeFlag::new()),
            fg,
            fg_key,
        })
    }

    /// A completed op enters the pipeline. Settlement happens on the next
    /// [`Vm::poll`], in completion order.
    pub fn settle(&mut self, triplet: Triplet) {
        self.queue.push(triplet);
    }

    pub fn pending(&self) -> usize {
        self.queue.pending()
    }

    pub fn stats(&self) -> crate::settle::Stats {
        self.queue.stats()
    }

    /// An async op starts on this Vm — the Rust-side face of what
    /// `__diting_spawn` does from JS. Its completion settles on a later
    /// [`Vm::poll`].
    pub fn spawn_op(&mut self, fut: impl Future<Output = Triplet> + 'static) {
        let driver = self
            .isolate
            .get_slot::<DriverSlot>()
            .expect("Vm::new installs the driver slot")
            .clone();
        driver.borrow_mut().spawn(fut);
    }

    /// Async ops in flight (started, no completion yet).
    pub fn pending_ops(&self) -> usize {
        self.isolate
            .get_slot::<DriverSlot>()
            .expect("Vm::new installs the driver slot")
            .borrow()
            .pending_ops()
    }

    /// Install sync ops onto the context's globalThis and return the shared
    /// state cell — hosts `put()` their state into it before first JS runs
    /// (`op_state().borrow_mut().put(..)` face). The op set is owned by the
    /// Vm for the isolate's lifetime (the Externals point into it).
    /// Enter/exit discipline: same as eval_string.
    pub fn install_ops(
        &mut self,
        decls: Vec<crate::op_reg::OpDecl>,
    ) -> std::rc::Rc<std::cell::RefCell<crate::state::OpState>> {
        let set = crate::op_reg::OpSet::new(decls);
        let state = set.state();
        unsafe { self.isolate.enter() };
        {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            let global = cs.get_current_context().global(cs);
            set.install_onto(cs, global);
        }
        unsafe { self.isolate.exit() };
        self.ops = Some(set);
        state
    }

    /// Install sync ops into a NAMESPACE OBJECT reached by walking
    /// `globalThis.path[0].path[1]…` (10d flip: diting's snapshot scripts
    /// capture `const _OPS = Deno.core.ops` at bake time — the ops must
    /// land in THAT object, never on globalThis, where they would become
    /// enumerable window properties and change the fingerprint face). The
    /// shell script bakes the empty namespace; every segment must exist
    /// here, or the host forgot to bake its shell — a loud panic, not a
    /// silent empty ops table.
    pub fn install_ops_under(
        &mut self,
        decls: Vec<crate::op_reg::OpDecl>,
        path: &[&str],
    ) -> std::rc::Rc<std::cell::RefCell<crate::state::OpState>> {
        let set = crate::op_reg::OpSet::new(decls);
        let state = set.state();
        unsafe { self.isolate.enter() };
        {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            let mut target = cs.get_current_context().global(cs);
            for seg in path {
                let key = v8::String::new(cs, seg)
                    .unwrap_or_else(|| panic!("namespace segment {seg} does not fit a string"));
                let next = target
                    .get(cs, key.into())
                    .and_then(|v| v.try_cast::<v8::Object>().ok())
                    .unwrap_or_else(|| {
                        panic!("namespace {seg} missing on the global object — bake the shell script before installing ops under it")
                    });
                target = next;
            }
            set.install_onto(cs, target);
        }
        unsafe { self.isolate.exit() };
        self.ops = Some(set);
        state
    }

    /// Raw isolate access under the enter/exit discipline — the host's
    /// isolate-level faces (near-heap-limit guard family) go through here.
    /// The closure must not run JS and must not stash the reference.
    pub fn with_isolate<R>(&mut self, f: impl FnOnce(&mut v8::OwnedIsolate) -> R) -> R {
        unsafe { self.isolate.enter() };
        let out = f(&mut self.isolate);
        unsafe { self.isolate.exit() };
        out
    }

    /// Install the module machinery: the map goes in the isolate slot,
    /// V8's module host callbacks get registered. Must run before any
    /// module face; scripts and ops work without it (a Vm that never sees
    /// ESM never pays for it). The slot is how the extern-C and
    /// builder-shape callbacks reach the map — same trick as the driver
    /// slot, one map per Vm (single-context isolate; deno_core needs a
    /// per-realm annex, we don't).
    pub fn install_modules(&mut self, loader: Rc<dyn ModuleLoader>) {
        let map = Rc::new(ModuleMap::new(loader));
        self.isolate.set_slot::<ModuleSlot>(map);
        self.isolate
            .set_host_import_module_dynamically_callback(dynamic::import_module_dynamically);
        self.isolate
            .set_host_initialize_import_meta_object_callback(
                initialize_import_meta_object_callback,
            );
    }

    /// Load a side ESM module whose ROOT source is given inline (the
    /// page's module code; its imports fetch through the loader). Drives
    /// the graph to completion — the embedder awaits this before
    /// evaluation; no event-loop pumping happens inside (no JS runs during
    /// a load). Returns the root's [`ModuleId`], instantiated and ready
    /// for [`Vm::mod_evaluate`].
    pub async fn load_side_es_module_from_code(
        &mut self,
        name: &str,
        code: String,
    ) -> Result<ModuleId, ModuleEvalError> {
        let map = self
            .isolate
            .get_slot::<ModuleSlot>()
            .cloned()
            .expect("Vm::install_modules runs before module loads");
        let mut load = RecursiveLoad::new_side(map.clone(), name, Some(code));
        let Vm {
            isolate, context, ..
        } = self;
        // `context` destructures as &mut Global<Context>, which has no
        // Handle impl — reborrow to the shared reference the macro wants.
        let context: &v8::Global<v8::Context> = context;
        let map = map.clone();
        std::future::poll_fn(move |cx| {
            loop {
                match load.poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Err(e))) => {
                        return Poll::Ready(Err(ModuleEvalError::Threw(e.message().to_string())));
                    }
                    Poll::Ready(Some(Ok((url, code)))) => {
                        // Enter/exit per registration — see eval_string;
                        // awaits between passes mean the TLS slot cannot be
                        // held across the whole drive.
                        unsafe { isolate.enter() };
                        let r = {
                            v8::scope_with_context!(cs, isolate, context);
                            load.register(cs, &url, &code)
                        };
                        unsafe { isolate.exit() };
                        if let Err(e) = r {
                            let e = module_error_to_eval(isolate, context, e);
                            return Poll::Ready(Err(e));
                        }
                    }
                    Poll::Ready(None) => {
                        let root = load.root_module_id().expect("root registered before Done");
                        unsafe { isolate.enter() };
                        let r = {
                            v8::scope_with_context!(cs, isolate, context);
                            map.instantiate_module(cs, root)
                        };
                        unsafe { isolate.exit() };
                        return match r {
                            Ok(()) => Poll::Ready(Ok(root)),
                            Err(e) => {
                                let e = module_error_to_eval(isolate, context, e);
                                Poll::Ready(Err(e))
                            }
                        };
                    }
                }
            }
        })
        .await
    }

    /// Evaluate an instantiated module. The returned future borrows
    /// NOTHING (the map is an Rc, the completion rides a oneshot) — the
    /// host pumps the event loop while awaiting it, exactly deno_core's
    /// `mod_evaluate` shape.
    pub fn mod_evaluate(&mut self, id: ModuleId) -> crate::modules::eval::ModEvaluateFuture {
        let map = self
            .isolate
            .get_slot::<ModuleSlot>()
            .cloned()
            .expect("Vm::install_modules runs before module evaluation");
        unsafe { self.isolate.enter() };
        let fut = {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            map.mod_evaluate(cs, id)
        };
        unsafe { self.isolate.exit() };
        fut
    }

    /// True while a static module evaluation (TLA) is in flight — the
    /// host loop's keep-pumping condition.
    pub fn has_pending_module_evaluation(&self) -> bool {
        self.isolate
            .get_slot::<ModuleSlot>()
            .map(|m| m.has_pending_module_evaluation())
            .unwrap_or(false)
    }

    /// True while any dynamic import is unresolved (loading, evaluating,
    /// or waiting on TLA).
    pub fn has_pending_dynamic_imports(&self) -> bool {
        self.isolate
            .get_slot::<ModuleSlot>()
            .map(|m| m.has_pending_dynamic_imports())
            .unwrap_or(false)
    }

    /// One module-progress pass with the CALLER's waker — the face a
    /// reactor-driven host drives (its loop waker lands in every module
    /// future; module-side wakeups wake the loop). The poll-driven
    /// [`Vm::poll`] wraps this with an advisory flag waker.
    pub fn poll_modules(&mut self, cx: &mut Context<'_>) -> Result<(), LoopError> {
        let Some(map) = self.isolate.get_slot::<ModuleSlot>().cloned() else {
            return Ok(());
        };
        unsafe { self.isolate.enter() };
        let r = {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            map.poll_progress(cx, cs)
        };
        unsafe { self.isolate.exit() };
        r.map_err(|e| match e {
            ModuleEvalError::Terminated => LoopError::Terminated,
            ModuleEvalError::Threw(m) => LoopError::TickThrew(m),
        })
    }

    /// Advisory: a module-load future woke since the last take (the
    /// poll-driven default waker's flag).
    pub fn take_module_wake(&mut self) -> bool {
        self.module_wake.take()
    }

    /// The watchdog arm: any JS currently running stops at the next
    /// checkpoint (uncatchable). Triplets already queued are untouched —
    /// parking is the queue's job. The interrupt itself is one-shot
    /// (pinned in tests); [`Vm::revive`] is the belt-and-suspenders cancel.
    pub fn terminate(&mut self) {
        self.isolate.terminate_execution();
    }

    /// The recovery arm: clear the terminate flag. The next [`Vm::poll`]
    /// re-delivers whatever parked — this pair is the closed loop.
    pub fn revive(&mut self) {
        self.isolate.cancel_terminate_execution();
    }

    /// Cross-thread terminate/cancel handle (the watchdog family's face —
    /// v8-native `IsolateHandle`, same type deno handed out through
    /// `v8_isolate().thread_safe_handle()`).
    pub fn isolate_handle(&self) -> v8::IsolateHandle {
        self.isolate.thread_safe_handle()
    }

    /// Evaluate a named script and keep the result VALUE (the embedder's
    /// execute_script — deno's shape). The name rides the script origin, so
    /// it surfaces in Error.stack for failing scripts; `eval_string` keeps
    /// frames origin-less for the driver/test surface. Terminations and
    /// exceptions map like every other entry face.
    pub fn execute_script(
        &mut self,
        name: &str,
        source: &str,
    ) -> Result<v8::Global<v8::Value>, LoopError> {
        let Vm {
            isolate, context, ..
        } = self;
        // Enter/exit discipline — see eval_string.
        unsafe { isolate.enter() };
        let result = execute_script_impl(isolate, context, name, source);
        unsafe { isolate.exit() };
        result
    }

    /// A handle scope entered on the vm's context — the `deno_core::scope!`
    /// shape the host uses to convert Globals (v8_to_json class of work).
    /// The closure's current context is the vm's; every Local it makes dies
    /// at the boundary, so return data or Globals, not Locals.
    pub fn with_scope<R>(&mut self, f: impl FnOnce(&mut v8::PinScope<'_, '_>) -> R) -> R {
        let isolate = &mut self.isolate;
        // Enter/exit discipline — see eval_string.
        unsafe { isolate.enter() };
        let out = {
            v8::scope_with_context!(cs, isolate, &self.context);
            f(cs)
        };
        unsafe { isolate.exit() };
        out
    }

    /// One loop pass: drive async op futures (completions enter the
    /// queue), deliver queued settlements into JS, then drain microtasks
    /// (promise continuations queued by this round's resolves).
    ///
    /// The driver borrow ends before delivery on purpose: JS that runs
    /// during settle/drain may spawn new ops, and the spawn callback needs
    /// the driver free (pinned by the chained-op test).
    ///
    /// `Ok(true)` = something was delivered. `Err` = a batch parked; the
    /// pass stops there — under termination the microtask drain would
    /// refuse anyway, and after a tick throw the poison policy owns what
    /// happens next. The drive still ran: an op completing under terminate
    /// parks in the queue instead of evaporating (#226/#240, the law).
    pub fn poll(&mut self) -> Result<bool, LoopError> {
        let flag_waker = {
            let driver = self
                .isolate
                .get_slot::<DriverSlot>()
                .expect("Vm::new installs the driver slot")
                .clone();
            // The sync face's "waker" is the advisory flag (see
            // register_loop_waker): spawn wakes the flag, the poll-driven
            // embedder re-drives until idle.
            let flag_waker = driver.borrow().flag_waker();
            driver
                .borrow_mut()
                .register_loop_waker(&flag_waker.clone());
            driver.borrow_mut().drive(&mut self.queue);
            flag_waker
        };
        // Same flag wakes for foreground posts on the sync face.
        crate::foreground::set_waker(&self.fg, &flag_waker);
        // Module progress (dynamic imports) rides between the Rust drive
        // and JS delivery: its loads are Rust-side futures like ops, its
        // settlements land in JS like triplets.
        {
            let waker = self.module_wake.waker();
            let mut cx = Context::from_waker(&waker);
            self.poll_modules(&mut cx)?;
        }
        // Enter/exit discipline — see eval_string. The early-return on a
        // parked batch must NOT skip the exit, so the drain moves inside.
        unsafe { self.isolate.enter() };
        // Pre-phase: drain foreground tasks (same law as poll_event_loop —
        // the sync embedder parks between polls, so every queued post runs
        // now or never).
        let _ran_fg = crate::foreground::drain(&self.fg);
        let delivered = self.deliver_round();
        if delivered.is_ok() {
            self.drain_microtasks();
            self.dispatch_rejections()?;
        }
        unsafe { self.isolate.exit() };
        delivered
    }

    /// The reactor face (10d): one pass driven by the CALLER's waker. Op
    /// bodies parked on tokio (timers, network) receive `cx`'s waker through
    /// `drive_with`, so the reactor wakes this loop directly — a parked
    /// host task needs no polling cadence of its own. Module machinery
    /// gets the same waker. Everything else is a `poll` pass.
    ///
    /// The host parks in its own `poll_fn` on `LoopActivity::idle() ==
    /// false`; a wake means "call me again".
    pub fn poll_event_loop(&mut self, cx: &mut Context<'_>) -> Result<LoopActivity, LoopError> {
        {
            let driver = self
                .isolate
                .get_slot::<DriverSlot>()
                .expect("Vm::new installs the driver slot")
                .clone();
            // deno's state.waker.register(cx.waker()) law: spawns from inside
            // a pass (and completions racing the park) re-schedule THIS task.
            driver.borrow_mut().register_loop_waker(cx.waker());
            driver.borrow_mut().drive_with(&mut self.queue, cx.waker());
        }
        // Foreground posts wake this same task (deno setup.rs law).
        crate::foreground::set_waker(&self.fg, cx.waker());
        self.poll_modules(cx)?;
        // Enter/exit discipline — see eval_string. The early-return on a
        // parked batch must NOT skip the exit, so the drain moves inside.
        unsafe { self.isolate.enter() };
        // Pre-phase: drain foreground tasks (deno poll_event_loop phase 0).
        // Runs BEFORE any delivery — an async wasm compile resolving here is
        // what unblocks the promises this pass is about to settle.
        let _ran_fg = crate::foreground::drain(&self.fg);
        let delivered = self.deliver_round();
        if delivered.is_ok() {
            self.drain_microtasks();
            self.dispatch_rejections()?;
        }
        unsafe { self.isolate.exit() };
        let delivered = delivered?;
        let pending_rejections = self
            .isolate
            .get_slot::<RejectionSlot>()
            .map(|slot| {
                let st = slot.borrow();
                !st.pending.is_empty() || !st.pending_handled.is_empty()
            })
            .unwrap_or(false);
        // V8's own counter + our queue: both faces of "engine work still in
        // flight" (background threads compiling, foreground posts waiting).
        let pending_background = self.isolate.has_pending_background_tasks()
            || crate::foreground::has_pending(&self.fg);
        Ok(LoopActivity {
            delivered,
            pending_ops: self.pending_ops(),
            pending_modules: self.has_pending_module_evaluation()
                || self.has_pending_dynamic_imports(),
            pending_rejections,
            pending_background,
        })
    }

    fn deliver_round(&mut self) -> Result<bool, LoopError> {
        let Vm {
            isolate,
            context,
            settle_fn,
            queue,
            ..
        } = self;
        queue.deliver(BATCH_CAP, |batch| {
            deliver_batch(isolate, context, settle_fn, batch)
        })
    }

    fn drain_microtasks(&mut self) {
        // No is_execution_terminating() gate here: that flag reads true only
        // WHILE the termination exception is unwinding — after our boundary
        // caught and cleared it, the flag is false whether or not the
        // terminate request is still armed (pinned in the tests). Delivery
        // failures are the loop's only terminate signal.
        v8::scope_with_context!(scope, &mut self.isolate, &self.context);
        scope.perform_microtask_checkpoint();
    }

    /// Phase 2c — fire pending promise-rejection events, AFTER the microtask
    /// drain (deno_core `jsruntime.rs::dispatch_rejections` law): handled
    /// pairs first (rejectionhandled fires before any later
    /// unhandledrejection), then the unhandled batch, swapped out WHOLE so
    /// rejections arriving inside the handlers queue for the next round.
    ///
    /// The handlers are read live from the context globals each round
    /// (PRELUDE registration face) — absent handler = drain silently (a bare
    /// V8 fired no event either). Handled-phase exceptions are ignored, deno
    /// law (`dispatch_handled_rejections` never opens a try-catch); an
    /// unhandled-phase exception or termination propagates to the loop with
    /// the un-fired remainder re-queued — deno drops the tail; we park it,
    /// the settle-queue law applied to its sibling queue.
    ///
    /// Enter/exit discipline: the caller holds the isolate entered (both
    /// `poll` faces call this inside their enter/exit block).
    fn dispatch_rejections(&mut self) -> Result<(), LoopError> {
        let Some(slot) = self.isolate.get_slot::<RejectionSlot>() else {
            return Ok(());
        };
        let slot = slot.clone();

        // Phase 1 — rejectionhandled. Pop-per-item: the borrow must be DOWN
        // while JS runs (the promise-reject callback re-enters the slot from
        // any rejection born inside a listener).
        loop {
            let item = slot.borrow_mut().pending_handled.pop_front();
            let Some((promise, reason)) = item else { break };
            let handler = {
                v8::scope_with_context!(cs, &mut self.isolate, &self.context);
                read_rejection_handler(cs, "__ditingHandledRejectionHandler")
            };
            if let Some(handler) = handler {
                v8::scope_with_context!(cs, &mut self.isolate, &self.context);
                let f = v8::Local::new(cs, &handler);
                let recv: v8::Local<v8::Value> = v8::undefined(cs).into();
                let args: [v8::Local<v8::Value>; 2] = [
                    v8::Local::<v8::Promise>::new(cs, &promise).into(),
                    v8::Local::new(cs, &reason),
                ];
                // Return ignored — see the law note above.
                let _ = f.call(cs, recv, &args);
            }
        }

        // Phase 2 — unhandled, swap out whole.
        let batch: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)> = {
            let mut st = slot.borrow_mut();
            std::mem::take(&mut st.pending).into()
        };
        if batch.is_empty() {
            return Ok(());
        }
        let handler = {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            read_rejection_handler(cs, "__ditingUnhandledRejectionHandler")
        };
        let Some(handler) = handler else {
            return Ok(()); // nobody listens: drained, like a bare V8
        };

        let mut failure: Option<Option<String>> = None;
        {
            v8::scope_with_context!(cs, &mut self.isolate, &self.context);
            v8::tc_scope!(let tc, cs);
            let f = v8::Local::new(tc, &handler);
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            for (i, (promise, reason)) in batch.iter().enumerate() {
                let args: [v8::Local<v8::Value>; 2] = [
                    v8::Local::<v8::Promise>::new(tc, promise).into(),
                    v8::Local::new(tc, reason),
                ];
                if f.call(tc, recv, &args).is_none() {
                    // Park the un-fired remainder, order kept, ahead of
                    // anything queued during the handler run.
                    let mut st = slot.borrow_mut();
                    for (p, r) in batch[i..].iter().rev() {
                        st.pending.push_front((p.clone(), r.clone()));
                    }
                    failure = Some(if tc.has_terminated() {
                        None
                    } else {
                        Some(match tc.message() {
                            Some(m) => m.get(tc).to_rust_string_lossy(tc),
                            None => "<no message>".into(),
                        })
                    });
                    break;
                }
            }
        }
        match failure {
            None => Ok(()),
            Some(None) => Err(LoopError::Terminated),
            Some(Some(msg)) => Err(LoopError::TickThrew(msg)),
        }
    }

    /// Run a script in the context and return its value as a string
    /// (`String(v)` semantics; null and undefined stay distinct). The
    /// driver/test probe surface.
    pub fn eval_string(&mut self, source: &str) -> Result<String, LoopError> {
        let Vm {
            isolate, context, ..
        } = self;
        // Enter/exit discipline: in this v8 crate an OwnedIsolate is
        // ENTERED at construction and stays the thread's current isolate
        // until dropped — the NEWEST live isolate wins the TLS slot. With
        // more than one Vm alive (one blob, many restores — the dormant
        // shell shape), flipping the entry back before our scopes open is
        // on us; ContextScope::new checks TLS current, not ownership.
        // Balanced around the whole call, callbacks included.
        unsafe { isolate.enter() };
        let result = eval_string_impl(isolate, context, source);
        unsafe { isolate.exit() };
        result
    }
}

/// A module-machinery failure → the host's message-shaped error face.
/// Live exceptions string-ify (`String(v)` law, same as eval_string);
/// termination stays termination.
fn module_error_to_eval(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    error: ModuleError,
) -> ModuleEvalError {
    match error {
        ModuleError::Terminated => ModuleEvalError::Terminated,
        ModuleError::Loader(e) => ModuleEvalError::Threw(e.message().to_string()),
        ModuleError::Exception(exception) => {
            v8::scope_with_context!(cs, isolate, context);
            let exception = v8::Local::new(cs, exception);
            let message = exception
                .to_string(cs)
                .unwrap_or_else(|| v8::String::new(cs, "<exception>").unwrap());
            ModuleEvalError::Threw(message.to_rust_string_lossy(cs))
        }
    }
}

fn eval_string_impl(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    source: &str,
) -> Result<String, LoopError> {
    v8::scope_with_context!(cs, isolate, context);
    v8::tc_scope!(let tc, cs);
    let src = v8::String::new(tc, source).expect("script source fits");
    let value = match v8::Script::compile(tc, src, None).and_then(|script| script.run(tc)) {
        Some(v) => v,
        None if tc.has_terminated() => return Err(LoopError::Terminated),
        None => {
            let msg = match tc.message() {
                Some(m) => m.get(tc).to_rust_string_lossy(tc),
                None => "<no message>".into(),
            };
            return Err(LoopError::TickThrew(msg));
        }
    };
    // String(value) coercion through the global constructor.
    let key = v8::String::new(tc, "String").unwrap();
    let string_fn = tc
        .get_current_context()
        .global(tc)
        .get(tc, key.into())
        .and_then(|f| f.try_cast::<v8::Function>().ok())
        .expect("String global exists");
    let coerced = string_fn
        .call(tc, v8::undefined(tc).into(), &[value])
        .expect("String() of a primitive does not throw");
    let s = coerced
        .try_cast::<v8::String>()
        .expect("String() returns string");
    Ok(s.to_rust_string_lossy(tc))
}

/// [`Vm::execute_script`]'s body: same entry mapping as
/// `eval_string_impl`, but the value survives as a Global and the script
/// carries the caller's name in its origin.
fn execute_script_impl(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    name: &str,
    source: &str,
) -> Result<v8::Global<v8::Value>, LoopError> {
    v8::scope_with_context!(cs, isolate, context);
    v8::tc_scope!(let tc, cs);
    let src = v8::String::new(tc, source).expect("script source fits");
    let name_s = v8::String::new(tc, name).expect("script name fits");
    let origin = v8::ScriptOrigin::new(
        tc,
        name_s.into(),
        0,
        0,
        false,
        -1,
        None,
        false,
        false,
        false,
        None,
    );
    let value = match v8::Script::compile(tc, src, Some(&origin)).and_then(|s| s.run(tc)) {
        Some(v) => v,
        None if tc.has_terminated() => return Err(LoopError::Terminated),
        None => {
            let msg = match tc.message() {
                Some(m) => m.get(tc).to_rust_string_lossy(tc),
                None => "<no message>".into(),
            };
            return Err(LoopError::TickThrew(msg));
        }
    };
    Ok(v8::Global::new(tc, value))
}

/// Deliver one batch: per-triplet `__diting_settle(id, ok, payload)` calls.
/// A prefix may already have settled on a failed earlier attempt — the JS
/// side absorbs the re-delivery (Map miss) — so the whole batch re-feeds.
fn deliver_batch(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    settle_fn: &v8::Global<v8::Function>,
    batch: &[Triplet],
) -> Result<(), LoopError> {
    // What tripped the first failing call. None-variant = the termination
    // interrupt; Some = a JS exception with its message.
    let mut failure: Option<Option<String>> = None;
    {
        v8::scope_with_context!(cs, isolate, context);
        v8::tc_scope!(let tc, cs);
        let f = v8::Local::new(tc, settle_fn);
        for t in batch {
            let id = v8::Integer::new(tc, t.promise_id);
            let ok = v8::Boolean::new(tc, t.ok);
            let payload = v8::String::new(tc, &t.payload).expect("settle payload fits a V8 string");
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let args: [v8::Local<v8::Value>; 3] = [id.into(), ok.into(), payload.into()];
            if f.call(tc, recv, &args).is_none() {
                // has_terminated() is the precise signal: the termination
                // interrupt has no message, and is_execution_terminating()
                // is only reliable while the exception is still unwinding
                // (both pinned by the tripwire test below).
                failure = Some(if tc.has_terminated() {
                    None
                } else {
                    Some(match tc.message() {
                        Some(m) => m.get(tc).to_rust_string_lossy(tc),
                        None => "<no message>".into(),
                    })
                });
                break;
            }
        }
    }
    match failure {
        None => Ok(()),
        Some(None) => Err(LoopError::Terminated),
        Some(Some(msg)) => Err(LoopError::TickThrew(msg)),
    }
}

/// The live handler under a rejection-face global, if the page registered
/// one (PRELUDE's registration face stores it there — baked into the blob
/// with the registering bootstrap, so restores need no rehydration).
fn read_rejection_handler<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Global<v8::Function>> {
    let key = v8::String::new(scope, name)?;
    let global = scope.get_current_context().global(scope);
    global
        .get(scope, key.into())
        .and_then(|v| v.try_cast::<v8::Function>().ok())
        .map(|f| v8::Global::new(scope, f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V8-face tripwire (discovered 2026-10-17, same-thread arming): the
    /// terminate request is ONE-SHOT — the first call after arming throws
    /// the termination interrupt (no message), and the SECOND call, with no
    /// `cancel_terminate_execution` in between, succeeds. Also:
    /// `is_execution_terminating()` reads false both before the bite and
    /// after the exception cleared — it is only true while unwinding.
    ///
    /// The loop is correct under BOTH one-shot and sticky V8 (a failed
    /// delivery parks; the next poll re-drives; three failures poison-drop),
    /// so if this pin ever fails, the engine's terminate semantics drifted —
    /// go look, don't delete.
    #[test]
    fn v8_face_terminate_request_is_oneshot() {
        let mut vm = Vm::new();
        vm.terminate();
        assert!(
            matches!(vm.eval_string("1+1"), Err(LoopError::Terminated)),
            "first call after arming bites"
        );
        assert_eq!(
            vm.eval_string("2+2").unwrap(),
            "4",
            "second call without revive succeeds — the request was consumed"
        );
        vm.terminate();
        vm.revive();
        assert_eq!(vm.eval_string("3+3").unwrap(), "6", "revive stays harmless");
    }

    /// The settle path with no interruption: defer, complete, poll once —
    /// the promise resolves and its microtask continuation ran.
    #[test]
    fn settle_reaches_promise_and_drains_microtasks() {
        let mut vm = Vm::new();
        let id = vm.eval_string("globalThis.__diting_defer()").unwrap();
        let id: i32 = id.parse().unwrap();
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "null"
        );

        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "hello".into(),
        });
        assert!(vm.poll().unwrap(), "one round delivered the batch");
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "hello"
        );
        assert_eq!(vm.pending(), 0);
    }

    /// THE acceptance pin (plan 步骤 4): terminate lands while a completed
    /// op sits undelivered → the batch parks instead of evaporating →
    /// revive → next poll re-delivers → the promise settles. #226/#240
    /// closed at the layer that owns dispatch.
    #[test]
    fn terminate_parks_revive_replays_promise_settles() {
        let mut vm = Vm::new();
        let id: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();

        // Op completes Rust-side; delivery has not been attempted yet.
        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "survived".into(),
        });

        // Watchdog fires. Same thread, deterministic: nothing is executing,
        // so the flag arms and bites the NEXT JS call — the delivery itself.
        vm.terminate();
        assert!(matches!(vm.poll(), Err(LoopError::Terminated)));

        // The batch did NOT evaporate — it parked, whole.
        assert_eq!(vm.pending(), 1, "parked batch still counts as pending");
        assert_eq!(vm.stats().parked_batches, 1);
        assert_eq!(vm.stats().delivered, 0);
        // And JS never heard it:
        vm.revive();
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "null",
            "terminated round delivered nothing"
        );

        // Revived. The next pass re-delivers parked-first; the promise
        // settles and the microtask tap flips.
        assert!(vm.poll().unwrap());
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "survived"
        );
        assert_eq!(vm.pending(), 0);
        let s = vm.stats();
        assert_eq!(s.delivered, 1);
        assert_eq!(s.dropped_batches, 0);
    }

    /// Terminate arms BEFORE any triplet reaches JS: the whole batch parks
    /// (nothing half-settles), revive replays all of it, every id settles
    /// exactly once. Mid-batch interruption (prefix settled, tail parked)
    /// reduces to the same replay+absorb law, pinned directly below.
    #[test]
    fn terminate_before_delivery_parks_whole_batch_replay_settles_once() {
        let mut vm = Vm::new();
        let a: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();
        let b: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();

        vm.settle(Triplet {
            promise_id: a,
            ok: true,
            payload: "first".into(),
        });
        vm.settle(Triplet {
            promise_id: b,
            ok: true,
            payload: "second".into(),
        });

        vm.terminate();
        assert!(matches!(vm.poll(), Err(LoopError::Terminated)));
        vm.revive();

        assert!(vm.poll().unwrap());
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "second"
        );
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "0,0",
            "both ids settled exactly once on the replay — ring and map drained"
        );
        assert_eq!(vm.stats().delivered, 2);
    }

    /// The replay-safety floor, pinned directly: settling the same id twice
    /// (a parked prefix re-fed after the JS side already consumed it) is a
    /// no-op — second settle absorbs, no double-resolve, no throw.
    #[test]
    fn double_settle_of_consumed_id_absorbs() {
        let mut vm = Vm::new();
        let id: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();
        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "once".into(),
        });
        assert!(vm.poll().unwrap());
        // Re-feed the same triplet — the parked-batch replay shape.
        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "again".into(),
        });
        assert!(vm.poll().unwrap(), "absorb is not an error");
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "once",
            "a consumed id re-settles as a no-op"
        );
    }

    /// A batch with a stale id absorbs and the rest still settles.
    #[test]
    fn stale_id_absorbs_rest_settles() {
        let mut vm = Vm::new();
        let id: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();
        vm.settle(Triplet {
            promise_id: 999_001,
            ok: true,
            payload: "gone".into(),
        });
        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "kept".into(),
        });
        assert!(vm.poll().unwrap());
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "kept"
        );
    }

    /// Rejection face: ok=false settles through reject; the error tap
    /// flips instead of the resolve tap.
    #[test]
    fn reject_face() {
        let mut vm = Vm::new();
        let id: i32 = vm
            .eval_string("globalThis.__diting_defer()")
            .unwrap()
            .parse()
            .unwrap();
        vm.settle(Triplet {
            promise_id: id,
            ok: false,
            payload: "boom".into(),
        });
        assert!(vm.poll().unwrap());
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastRejected").unwrap(),
            "boom"
        );
        assert_eq!(
            vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
            "null"
        );
    }

    /// The async op roundtrip: JS calls __diting_op_sleep → the raw spawn
    /// callback starts an AfterTicks future in the driver → polls drive it
    /// to completion → the queue delivers → the promise settles → the
    /// microtask drain runs the continuation. ticks=2 = two passes pending,
    /// settlement on the second poll.
    #[test]
    fn op_roundtrip_through_future() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.r = null; \
             __diting_op_sleep(2, 'hi').then(function (v) { globalThis.r = v; })",
        )
        .unwrap();
        assert_eq!(vm.pending_ops(), 1, "the spawn callback started the op");
        assert_eq!(vm.eval_string("globalThis.r").unwrap(), "null");

        vm.poll().unwrap(); // tick 1 of 2: future still pending
        assert_eq!(
            vm.eval_string("globalThis.r").unwrap(),
            "null",
            "nothing settled a tick early"
        );
        assert_eq!(vm.pending_ops(), 1);

        vm.poll().unwrap(); // tick 2 of 2: completes, delivers, drains
        assert_eq!(vm.eval_string("globalThis.r").unwrap(), "hi");
        assert_eq!(vm.pending_ops(), 0);
    }

    /// The money pin — #226's exact shape with a real op: watchdog
    /// terminates while an op is in flight; the op completes Rust-side
    /// anyway (the interrupt only bites JS); the completion parks in the
    /// queue instead of evaporating; revive re-delivers and the promise
    /// settles. deno_core drops this triplet on the floor at the same
    /// moment; that gap is why this crate exists.
    #[test]
    fn op_completing_under_terminate_parks_and_lands_on_revive() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.r = null; \
             __diting_op_sleep(1, 'late').then(function (v) { globalThis.r = v; })",
        )
        .unwrap();
        vm.terminate();

        // Pass under terminate: the drive runs (Rust is immune), the
        // completion lands in the queue, the delivery bites the one-shot
        // interrupt and parks.
        assert!(matches!(vm.poll(), Err(LoopError::Terminated)));
        assert_eq!(vm.pending(), 1, "the completion parked, not evaporated");
        assert_eq!(
            vm.pending_ops(),
            0,
            "the op itself finished — the queue is the survival layer"
        );

        vm.revive();
        assert!(vm.poll().unwrap());
        assert_eq!(vm.eval_string("globalThis.r").unwrap(), "late");
    }

    /// Completion order wins over spawn order: fast op settles first even
    /// though it was started second.
    #[test]
    fn multiple_ops_settle_in_completion_order() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.order = []; \
             __diting_op_sleep(3, 'slow').then(function (v) { order.push(v); }); \
             __diting_op_sleep(1, 'fast').then(function (v) { order.push(v); })",
        )
        .unwrap();
        assert_eq!(vm.pending_ops(), 2);
        for _ in 0..3 {
            vm.poll().unwrap();
        }
        assert_eq!(
            vm.eval_string("globalThis.order.join(',')").unwrap(),
            "fast,slow"
        );
        assert_eq!(vm.pending_ops(), 0);
    }

    /// The RefCell discipline pin: a continuation that runs during the
    /// microtask drain spawns the next op — the loop must have dropped its
    /// driver borrow before any JS ran, or this callback would deadlock
    /// (borrow_mut inside borrow_mut). Chained ops work exactly like the
    /// chained timers real pages run.
    #[test]
    fn op_chained_from_continuation_spawns_next_pass() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.done = false; \
             __diting_op_sleep(1, 'a').then(function () { \
               __diting_op_sleep(1, 'b').then(function () { globalThis.done = true; }); \
             })",
        )
        .unwrap();

        vm.poll().unwrap(); // 'a' settles; the drain runs the continuation → spawns 'b'
        assert_eq!(
            vm.pending_ops(),
            1,
            "the continuation spawned op 'b' during the microtask drain"
        );
        assert_eq!(vm.eval_string("globalThis.done").unwrap(), "false");

        vm.poll().unwrap(); // 'b' completes this pass, settles, drain flips the flag
        assert_eq!(vm.eval_string("globalThis.done").unwrap(), "true");
    }

    /// Ring law 1 — eviction: more concurrent promises than ring slots.
    /// The first RING_SIZE+ entries keep their slots; the extra ten evict
    /// the OLDEST occupants of their slots into the overflow map. All of
    /// them settle, settled in reverse order (map path and ring path
    /// interleaved).
    #[test]
    fn ring_overflow_evicts_to_map_and_all_settle() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.count = 0; globalThis.ids = []; \
             for (let i = 0; i < RING_SIZE + 10; i++) { \
               const r = __diting_register(); \
               r.promise.then(function () { globalThis.count++; }); \
               globalThis.ids.push(r.id); \
             } \
             globalThis.__diting_tableCounts()",
        )
        .unwrap();
        // 4096 ring slots hold the LAST 4096 registrations; the first 10
        // were evicted to the map when their slots got reclaimed.
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "4096,10"
        );

        vm.eval_string(
            "for (let i = globalThis.ids.length - 1; i >= 0; i--) { \
               __diting_settle(globalThis.ids[i], true, 'x'); \
             }",
        )
        .unwrap();
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "0,0",
            "every promise taken, both ring and map drained"
        );
        vm.poll().unwrap(); // drain the 4106 microtask continuations
        assert_eq!(vm.eval_string("globalThis.count").unwrap(), "4106");
    }

    /// Ring law 2 — slot collision: two live promises whose ids differ by
    /// exactly RING_SIZE land on the same slot. The second takes the ring
    /// slot; the first moves to the map. Both settle through their own
    /// path.
    #[test]
    fn ring_same_slot_two_live_ids_both_settle() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.count = 0; \
             __diting_setNextId(5); \
             const a = __diting_register(); \
             __diting_setNextId(5 + RING_SIZE); \
             const b = __diting_register(); \
             a.promise.then(function () { globalThis.count++; }); \
             b.promise.then(function () { globalThis.count++; }); \
             __diting_settle(a.id, true, 'a'); \
             __diting_settle(b.id, true, 'b');",
        )
        .unwrap();
        vm.poll().unwrap(); // drain the two continuations
        assert_eq!(
            vm.eval_string("globalThis.count").unwrap(),
            "2",
            "a settled via the map, b via the ring — own paths, both land"
        );
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "0,0"
        );
    }

    /// Ring law 3 — the id guard: a settle for an id that maps to an
    /// occupied slot but is NOT the entry's own id must absorb WITHOUT
    /// clearing that slot (the entry[2] === id check). The ring's promise
    /// still settles afterwards.
    #[test]
    fn missing_id_on_occupied_slot_absorbs_without_disturbance() {
        let mut vm = Vm::new();
        vm.eval_string(
            "globalThis.count = 0; \
             const a = __diting_register(); \
             globalThis.firstId = a.id; \
             a.promise.then(function () { globalThis.count++; }); \
             __diting_settle(a.id + RING_SIZE, true, 'bogus');",
        )
        .unwrap();
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "1,0",
            "the bogus settle neither cleared the slot nor leaked an entry"
        );
        let id: i32 = vm
            .eval_string("globalThis.firstId")
            .unwrap()
            .parse()
            .unwrap();
        vm.settle(Triplet {
            promise_id: id,
            ok: true,
            payload: "real".into(),
        });
        vm.poll().unwrap();
        assert_eq!(
            vm.eval_string("globalThis.count").unwrap(),
            "1",
            "the real settle still lands through Rust delivery"
        );
    }

    /// Ring law 4 — wraparound: the id counter wraps at 2^31-1 back to 0
    /// (deno's async op stub arithmetic). Ids on both sides of the wrap
    /// settle normally.
    #[test]
    fn id_counter_wraps_at_i32_max() {
        let mut vm = Vm::new();
        let ids = vm
            .eval_string(
                "globalThis.count = 0; \
                 __diting_setNextId(0x7fffffff); \
                 const top = __diting_register(); \
                 const wrapped = __diting_register(); \
                 [top.id, wrapped.id].join(',')",
            )
            .unwrap();
        assert_eq!(
            ids, "2147483647,0",
            "the counter wrapped back to zero, both ids stay non-negative i32"
        );
        vm.eval_string(
            "top.promise.then(function () { globalThis.count++; }); \
             wrapped.promise.then(function () { globalThis.count++; }); \
             __diting_settle(2147483647, true, 'x'); \
             __diting_settle(0, true, 'x'); \
             globalThis.__diting_tableCounts()",
        )
        .unwrap();
        assert_eq!(
            vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
            "0,0"
        );
        vm.poll().unwrap();
        assert_eq!(vm.eval_string("globalThis.count").unwrap(), "2");
    }
}
