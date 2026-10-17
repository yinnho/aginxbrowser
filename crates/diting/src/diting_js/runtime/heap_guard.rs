use super::JsRuntime;
use diting_runtime::vm::Vm;
use std::cell::RefCell;
use v8::IsolateHandle;

/// How much the near-heap-limit callback raises the limit so V8 can unwind
/// the terminated script instead of aborting the process.
pub(super) const HEAP_LIMIT_RECOVERY_HEADROOM_BYTES: usize = 64 * 1024 * 1024;

/// #224: the first near-limit trip gets a wide recovery window and a
/// pressure GC instead of a kill — Chrome asks the GC for help before it
/// beheads anything. Only a page that climbs the ceiling AGAIN inside this
/// window is terminated (the old behavior, one GC later).
pub(super) const HEAP_PRESSURE_HEADROOM_BYTES: usize = 512 * 1024 * 1024;

#[derive(Default)]
pub(super) struct HeapLimitState {
    // pub(super): the runtime tests drive the strike counter by hand.
    pub(super) tripped: std::sync::atomic::AtomicBool,
    pub(super) restore_limit: std::sync::atomic::AtomicUsize,
    /// Set by the first near-limit trip, cleared by the safe-point recovery
    /// in `recover_heap_limit`. While set, the next trip terminates.
    pub(super) pressured: std::sync::atomic::AtomicBool,
    /// Terminations this realm has survived (hosted-incident 10-14): the
    /// pre-teardown guard terminated the current task, canceled, and let
    /// the session climb the wall again — one heavy SPA per realm rode that
    /// loop for hours and 7GB of RSS. The session actor polls
    /// [`JsRuntime::heap_guard_strikes`] and drops the realm on the third.
    strikes: std::sync::atomic::AtomicUsize,
}

/// V8's default response to hitting the heap limit is to abort the whole
/// process — with many sessions in one server, one page's allocation loop
/// would kill every session. Escalation, Chrome-shaped (#224):
///
/// 1. First trip: lend `HEAP_PRESSURE_HEADROOM_BYTES` and flag pressure —
///    no termination. The GC runs at the next safe point (isolate thread).
/// 2. Second trip before the safe point recovered: terminate the current
///    script (the pre-#224 behavior) and lend the smaller unwind headroom.
///
/// The closure V8's near-heap-limit callback resolves to. deno_core kept
/// the box in JsRuntimeState (its `allocations`); our Vm has no such annex,
/// so the box lives in an isolate slot — dropped with the isolate, and
/// re-installation takes the old one out only after the raw callback was
/// unregistered (the address is not guaranteed stable across a same-type
/// slot replace). Thunk shape verbatim from deno_core 0.411
/// `jsruntime.rs::near_heap_limit_callback` (MIT, 2018-2026 Deno authors).
struct HeapCbSlot(Box<RefCell<dyn FnMut(usize, usize) -> usize>>);

unsafe extern "C" fn near_heap_limit_thunk(
    data: *mut core::ffi::c_void,
    current_heap_limit: usize,
    initial_heap_limit: usize,
) -> usize {
    // SAFETY: `data` is the HeapCbSlot's slot-storage pointer, handed to V8
    // by install_heap_limit_guard below; the slot outlives every callback
    // firing because removal always precedes slot replacement.
    let slot = unsafe { &*(data as *const HeapCbSlot) };
    (slot.0.borrow_mut())(current_heap_limit, initial_heap_limit)
}

pub(super) fn install_heap_limit_guard(
    runtime: &mut Vm,
    isolate_handle: IsolateHandle,
    state: std::sync::Arc<HeapLimitState>,
) {
    let body = move |current_limit: usize, _initial_limit: usize| {
        let _ = state.restore_limit.compare_exchange(
            0,
            current_limit,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        // #50 probe: the second captured repro showed tick_fn terminating
        // with NO watchdog fire anywhere near it — this guard is the only
        // other silent terminate_execution() call site. Make it visible.
        let t_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if !state
            .pressured
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            eprintln!(
                "[heapguard] near-heap-limit at {} bytes (initial {}) — lending {}MB, pressure GC at the next safe point t={t_ms}",
                current_limit,
                _initial_limit,
                HEAP_PRESSURE_HEADROOM_BYTES / 1024 / 1024
            );
            return current_limit.saturating_add(HEAP_PRESSURE_HEADROOM_BYTES);
        }
        state.tripped.store(true, std::sync::atomic::Ordering::SeqCst);
        eprintln!(
            "[heapguard] near-heap-limit tripped at {} bytes (initial {}) — pressure GC did not free enough; terminating t={t_ms}",
            current_limit, _initial_limit
        );
        isolate_handle.terminate_execution();
        current_limit.saturating_add(HEAP_LIMIT_RECOVERY_HEADROOM_BYTES)
    };
    runtime.with_isolate(|isolate| {
        // Belt-and-suspenders: recover_heap_limit removes before re-arming,
        // but a future call path that forgets must not leave V8 holding a
        // data pointer into a dropped slot box.
        if isolate.get_slot::<HeapCbSlot>().is_some() {
            isolate.remove_near_heap_limit_callback(near_heap_limit_thunk, 0);
        }
        isolate.set_slot::<HeapCbSlot>(HeapCbSlot(Box::new(RefCell::new(body))));
        let data = isolate
            .get_slot::<HeapCbSlot>()
            .expect("just set")
            as *const HeapCbSlot
            as *mut core::ffi::c_void;
        isolate.add_near_heap_limit_callback(near_heap_limit_thunk, data);
    });
}

/// The remove half of [`install_heap_limit_guard`]: unregister the raw V8
/// callback AND take the slot out — the belt in install only removes when
/// the slot exists, so a remove that left the slot behind would fatal V8's
/// CHECK on the next re-arm (remove of an unregistered callback).
fn uninstall_heap_limit_guard(runtime: &mut Vm, restore_limit: usize) {
    runtime.with_isolate(|isolate| {
        if isolate.get_slot::<HeapCbSlot>().is_some() {
            isolate.remove_near_heap_limit_callback(near_heap_limit_thunk, restore_limit);
        }
        isolate.remove_slot::<HeapCbSlot>();
    });
}


impl JsRuntime {
    /// How many times the heap guard terminated a task in this realm's
    /// lifetime (hosted-incident 10-14). The session actor drops the realm
    /// — freeing the isolate and its heap — once this crosses its strike
    /// limit, instead of letting an allocation-looping page ride
    /// terminate→GC→climb forever.
    pub fn heap_guard_strikes(&self) -> usize {
        self.heap_limit_state
            .strikes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Safe-point recovery, run at evaluate / run_event_loop entry (same
    /// thread as the isolate). If the guard only flagged pressure, run the
    /// full low-memory GC and re-arm the guard at the real limit — the page
    /// survived. If it terminated, cancel the termination and restore
    /// (the pre-#224 recovery path).
    pub(super) fn recover_heap_limit(&mut self) -> bool {
        let pressured = self
            .heap_limit_state
            .pressured
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        let tripped = self
            .heap_limit_state
            .tripped
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if !pressured && !tripped {
            return false;
        }
        // The callback always runs with a positive limit, so this is the
        // original ceiling whenever either flag was set; a 0 restore would
        // poison the isolate, so guard anyway.
        let restore_limit = self
            .heap_limit_state
            .restore_limit
            .swap(0, std::sync::atomic::Ordering::SeqCst);
        if tripped {
            self.heap_limit_state
                .strikes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.runtime.revive();
            // #227: the guard's terminate_execution carries no stale flag,
            // so the #39/#50 heals are blind to it — a dispatch window
            // beheaded at the heap wall leaks _mtDepth and every later
            // macrotask chain defer-spins on it forever. Forward the unwind
            // for the next loop entry (this runs at loop entry already, so
            // the heal in the same pass consumes it).
            self.termination_unwound
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if restore_limit > 0 {
                uninstall_heap_limit_guard(&mut self.runtime, restore_limit);
            }
            install_heap_limit_guard(
                &mut self.runtime,
                self.isolate_handle.clone(),
                self.heap_limit_state.clone(),
            );
            tracing::warn!(
                "V8 heap limit reached: pressure GC did not free enough — terminated the current JavaScript task"
            );
        } else {
            // Pressure window survived: full GC here (blocking, safe point),
            // then the guard re-arms at the real ceiling for the next cycle.
            self.runtime
                .with_isolate(|isolate| isolate.low_memory_notification());
            if restore_limit > 0 {
                uninstall_heap_limit_guard(&mut self.runtime, restore_limit);
            }
            install_heap_limit_guard(
                &mut self.runtime,
                self.isolate_handle.clone(),
                self.heap_limit_state.clone(),
            );
            tracing::warn!(
                "V8 heap near-limit: pressure GC ran at the safe point ({}MB headroom was lent)",
                HEAP_PRESSURE_HEADROOM_BYTES / 1024 / 1024
            );
        }
        true
    }
}
