//! The V8 vertical slice — a bare isolate driven by our own loop.
//!
//! Batch 2 of the crate's roadmap: prove the seam end to end on raw `v8`,
//! with no deno_core anywhere. One isolate, one context, one JS settlement
//! face, and a loop pass that runs the [`SettleQueue`] into JS and drains
//! microtasks. The acceptance pin is the campaign's minimal closed loop:
//! terminate lands mid-settlement → the batch parks instead of evaporating
//! → `cancel_terminate_execution` revives the isolate → the next poll
//! re-delivers → the awaiting promise settles. The #226/#240 shape, owned.
//!
//! What this slice deliberately does NOT have yet: the immediates/close
//! phases (diting uses neither — no setImmediate, no close callbacks
//! anywhere in the tree) and the ring promise table (the JS face here is a
//! Map — the seed, same law). Those are later batches; building them now
//! would be scaffolding without a test behind it. The op driver (batch 3)
//! IS wired in: async futures driven each pass, completions parking in the
//! settle queue — see [`crate::driver`].

use crate::driver::{AfterTicks, OpDriver};
use crate::settle::SettleQueue;
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Once;

/// Triplets per delivery batch. deno_core ships the whole completed-ops vec
/// in one `__eventLoopTick` call; we settle per-triplet (the JS claim layer
/// in js/core_base.js already proved per-triplet calls are noise: ≤341 in
/// the worst real trace, against a cross-FFI boundary the settle ring pays
/// anyway). 512 keeps batches bounded without a second trip through the
/// queue.
const BATCH_CAP: usize = 512;

/// The JS face of settlement — the seed of our own promise table. Kept in
/// sync with the Rust call in [`deliver_batch`]: (id, ok, payload).
/// Missing ids absorb natively (Map lookup miss → return), which is the
/// same Missing-absorb law js/core_base.js implements against deno's ring.
const PRELUDE: &str = r#"
"use strict";
globalThis.__diting_promises = new Map();
globalThis.__diting_nextId = 1;
globalThis.__diting_lastResolved = null;
globalThis.__diting_lastRejected = null;
globalThis.__diting_settle = function (id, ok, payload) {
  const entry = globalThis.__diting_promises.get(id);
  if (entry === undefined) return; // stale or unknown: absorb, keep delivering
  globalThis.__diting_promises.delete(id);
  if (ok) entry.resolve(payload);
  else entry.reject(new Error(payload));
};
// Driver probe: register a promise in the table, return its id. The .then
// taps make settlement observable from Rust without extra plumbing — the
// microtask drain flips the global, eval reads it.
globalThis.__diting_defer = function () {
  const id = globalThis.__diting_nextId++;
  const p = new Promise(function (resolve, reject) {
    globalThis.__diting_promises.set(id, { resolve, reject });
  });
  p.then(
    function (v) { globalThis.__diting_lastResolved = String(v); },
    function (e) { globalThis.__diting_lastRejected = String(e && e.message || e); }
  );
  return id;
};
// The async op face: register a promise, hand the op to Rust
// (__diting_spawn is a raw native callback installed from the Rust side),
// return the promise. This is the op_sleep shape bootstrap.js runs page
// timers on; here it drives AfterTicks futures inside the driver.
globalThis.__diting_op_sleep = function (ticks, payload) {
  const id = globalThis.__diting_nextId++;
  let resolve, reject;
  const p = new Promise(function (res, rej) { resolve = res; reject = rej; });
  globalThis.__diting_promises.set(id, { resolve, reject });
  globalThis.__diting_spawn(id, ticks, String(payload));
  return p;
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
        v8::V8::initialize_platform(v8::new_default_platform(0, false).make_shared());
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

/// The driver lives in an isolate slot behind Rc<RefCell>: raw V8 function
/// callbacks have no closure capture, so `__diting_spawn` recovers the
/// isolate from the callback parts and finds the driver here (deno's ops
/// reach their OpCtx external the same way). RefCell because spawn can
/// fire from inside JS microtasks while the loop holds no borrow — the
/// loop drops its borrow before any JS runs (pinned by a test).
type DriverSlot = Rc<RefCell<OpDriver>>;

/// `__diting_spawn(id, ticks, payload)` — the JS→Rust op entry.
///
/// SAFETY: V8 invokes this under the API-callback contract (valid
/// FunctionCallbackInfo pointer, an open handle scope, on the isolate's
/// thread). We only touch the isolate through the recovered reference and
/// never outlive the call. No panics across this boundary — a panic inside
/// `extern "C"` is UB (abort), so a missing slot is a silent no-op: the
/// promise would hang, and every driver test would fail loudly anyway.
unsafe extern "C" fn spawn_callback(info: *const v8::FunctionCallbackInfo) {
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

/// A bare V8 isolate driven by our loop. Single-threaded by design, like
/// everything that lives next to an isolate.
pub struct Vm {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    settle_fn: v8::Global<v8::Function>,
    queue: SettleQueue<Triplet>,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    pub fn new() -> Self {
        ensure_v8();
        let mut isolate = v8::Isolate::new(Default::default());
        // Microtasks run exactly when the loop drains them — never
        // implicitly at some V8-internal checkpoint. The loop owns the
        // schedule.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        // The op driver goes in before any JS runs — __diting_spawn finds
        // it through this slot.
        isolate.set_slot::<DriverSlot>(Rc::new(RefCell::new(OpDriver::new())));

        let context = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            v8::Global::new(scope, context)
        };
        let settle_fn = {
            v8::scope_with_context!(cs, &mut isolate, &context);
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

            let name = v8::String::new(cs, "__diting_settle").unwrap();
            let settle_fn = global
                .get(cs, name.into())
                .and_then(|f| f.try_cast::<v8::Function>().ok())
                .expect("prelude installs __diting_settle");
            v8::Global::new(cs, settle_fn)
        };

        Self {
            isolate,
            context,
            settle_fn,
            queue: SettleQueue::new(),
        }
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
        {
            let driver = self
                .isolate
                .get_slot::<DriverSlot>()
                .expect("Vm::new installs the driver slot")
                .clone();
            driver.borrow_mut().drive(&mut self.queue);
        }
        let delivered = self.deliver_round()?;
        self.drain_microtasks();
        Ok(delivered)
    }

    fn deliver_round(&mut self) -> Result<bool, LoopError> {
        let Vm {
            isolate,
            context,
            settle_fn,
            queue,
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

    /// Run a script in the context and return its value as a string
    /// (`String(v)` semantics; null and undefined stay distinct). The
    /// driver/test probe surface.
    pub fn eval_string(&mut self, source: &str) -> Result<String, LoopError> {
        let Vm {
            isolate, context, ..
        } = self;
        v8::scope_with_context!(cs, isolate, &*context);
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
            vm.eval_string("globalThis.__diting_promises.size").unwrap(),
            "0",
            "both ids settled exactly once on the replay"
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
}
