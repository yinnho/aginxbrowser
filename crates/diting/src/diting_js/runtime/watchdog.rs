//! V8 execution watchdogs: the armed-token family behind
//! [`JsRuntime::arm_watchdog`](super::JsRuntime::arm_watchdog) — the cancel
//! channel + join handle, the debug stack-dump sampler
//! (AGINXBROWSER_WATCHDOG_STACK_DUMP=1), and `spawn_watchdog` for callers
//! holding a bare isolate handle. Split from the module root (god-file
//! ratchet).
use deno_core::v8::{self, IsolateHandle};

/// Handle to an armed V8 execution watchdog (see [`JsRuntime::arm_watchdog`]).
/// Holds the cancel channel and the watchdog thread; pass it back to
/// `disarm_watchdog` to stop the watchdog and learn whether it fired.
pub struct WatchdogToken {
    pair: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    join: Option<std::thread::JoinHandle<()>>,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Debug knob (AGINXBROWSER_WATCHDOG_STACK_DUMP=1): a sampler thread that
    /// requests a stack-dump interrupt every 250ms while this watchdog is
    /// armed, so the FIRST sample lands mid-spin — long before any terminate
    /// (and its subsequent recovery scripts) destroys the stack we want.
    /// `None` when the knob is off.
    sampler_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Debug knob (AGINXBROWSER_WATCHDOG_STACK_DUMP=1): dump the live JS stack of
/// a synchronous overrun. Two producers share this callback:
///  - `watchdog_terminate` requests one at fire time (best effort: if the spin
///    is inside a Rust op, no interrupt can land until the op returns, and a
///    terminate from another watchdog may have already unwound the stack);
///  - the per-watchdog sampler requests one every 250ms while armed, so the
///    first samples land MID-SPIN, which is the trustworthy view.
///
/// `data` points at a boxed `u64` request timestamp (ms since epoch) owned by
/// the requester; null means fire-time (the box would be unowned anyway).
/// Off by default; strictly an engine-debugging aid.
fn stack_dump_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

unsafe extern "C" fn dump_spin_stack(
    isolate: v8::UnsafeRawIsolatePtr,
    data: *mut std::ffi::c_void,
) {
    let requested_ms = if data.is_null() {
        None
    } else {
        let ms = unsafe { *std::ptr::NonNull::new(data).unwrap().cast::<u64>().as_ptr() };
        drop(unsafe { Box::from_raw(data as *mut u64) });
        Some(ms)
    };
    let landed_ms = stack_dump_now_ms();
    let mut isolate = unsafe { v8::Isolate::from_raw_isolate_ptr(isolate) };
    let terminating = isolate.is_execution_terminating();
    let tag = match requested_ms {
        Some(req) => {
            let lag = landed_ms.saturating_sub(req);
            format!("sample lag={lag}ms")
        }
        None => "fire".to_string(),
    };
    eprintln!("[watchdog-stack] {landed_ms} {tag} interrupt landed; terminating={terminating}");
    v8::scope!(let hs, &mut isolate);
    // SAFETY: `HandleScope<'i, C>`'s C parameter exists only as PhantomData,
    // so HandleScope<'i, ()> and HandleScope<'i, Context> share a layout, and
    // re-borrowing the live HandleScope as the Context-flavored PinScope the
    // StackTrace API wants is sound. An earlier version entered a synthetic
    // Context here (Context::new + ContextScope) and capture saw 0 frames;
    // entering no context at all rules that interplay out.
    let scope: &v8::PinScope<'_, '_> = unsafe { std::mem::transmute(&*hs) };
    let Some(trace) = v8::StackTrace::current_stack_trace(scope, 24) else {
        eprintln!("[watchdog-stack] {landed_ms} no stack available");
        return;
    };
    let count = trace.get_frame_count().min(24);
    eprintln!("[watchdog-stack] {landed_ms} {count} frames (top first):");
    for i in 0..count {
        let Some(frame) = trace.get_frame(scope, i) else { continue };
        let fn_name = frame
            .get_function_name(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        let script = frame
            .get_script_name(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        let fn_name = if fn_name.is_empty() { "<anon>" } else { &fn_name };
        // Minified bundles are one line, so name+line:col can't tell 54 inline
        // scripts apart — the V8 script id (compile order) plus a source
        // prefix can. Prefix only on the top frame to keep samples cheap.
        let source_tag = if i == 0 {
            let prefix: String = frame
                .get_script_source(scope)
                .map(|s| {
                    s.to_rust_string_lossy(scope)
                        .chars()
                        .take(80)
                        .map(|c| if c == '\n' { ' ' } else { c })
                        .collect()
                })
                .unwrap_or_default();
            format!(" id={} src≈[{}]", frame.get_script_id(), prefix)
        } else {
            format!(" id={}", frame.get_script_id())
        };
        eprintln!(
            "[watchdog-stack]   #{i} {fn_name} @ {script}:{}:{}{source_tag}",
            frame.get_line_number(),
            frame.get_column()
        );
    }
}

/// Shared tail of every watchdog fire path. Honors the debug knob above
/// (request a stack dump, give the interrupt a moment to land mid-spin)
/// before terminating. The dump is best-effort: if the spin is inside a Rust
/// op the interrupt only lands after the op returns, and a normally-finishing
/// script just dumps wherever the next V8 entry is — harmless either way.
pub(super) fn watchdog_terminate(handle: &IsolateHandle) {
    if std::env::var("AGINXBROWSER_WATCHDOG_STACK_DUMP").is_ok() {
        // Fire-time dumps are the fallback view; the armed sampler (see
        // spawn_watchdog) is the trustworthy one. 150ms per attempt is plenty
        // for a V8-visible spin (interrupts land at the next stack check);
        // an op-bound spin can't be reached at all.
        handle.request_interrupt(dump_spin_stack, std::ptr::null_mut());
        std::thread::sleep(std::time::Duration::from_millis(150));
        handle.request_interrupt(dump_spin_stack, std::ptr::null_mut());
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    handle.terminate_execution();
}

/// Arm a V8 termination watchdog directly from an isolate handle, with no
/// runtime borrow. The CDP dispatcher uses this to bound every command so a
/// hung page cannot hold the process-wide V8 lock forever. Pair with
/// [`WatchdogToken::stop`]; if `stop` returns true, clear the termination flag
/// via [`JsRuntime::cancel_termination`] before reusing the isolate.
///
/// `stale` (the runtime's `stale_termination` flag) is set at the instant of
/// firing — BEFORE `terminate_execution()` — so `run_event_loop`'s poll-boundary
/// heal can tell a pending termination from V8 internals it cannot observe.
pub fn spawn_watchdog(
    handle: IsolateHandle,
    budget: std::time::Duration,
    stale: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> WatchdogToken {
    let pair = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Debug sampler (AGINXBROWSER_WATCHDOG_STACK_DUMP=1): while this watchdog
    // is armed, poke a stack-dump interrupt every 250ms. For a spin that V8
    // can see (JS loop, or a loop of op calls), samples land mid-spin —
    // before terminate_execution and the recovery scripts destroy the
    // evidence. A lag >>250ms on every sample means the thread was parked
    // inside a single Rust op — the one fact fire-time dumps can't show.
    let sampler_stop = if std::env::var("AGINXBROWSER_WATCHDOG_STACK_DUMP").is_ok() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sampler_handle = handle.clone();
        let sampler_stop = stop.clone();
        std::thread::spawn(move || {
            while !sampler_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let req = Box::into_raw(Box::new(stack_dump_now_ms()));
                sampler_handle.request_interrupt(dump_spin_stack, req.cast());
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
        Some(stop)
    } else {
        None
    };
    let pair_c = pair.clone();
    let fired_c = fired.clone();
    let join = std::thread::spawn(move || {
        let (lock, cvar) = &*pair_c;
        let mut cancelled = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + budget;
        loop {
            // Check first: stop() may have set this (and notified into the void)
            // before this thread even started, which happens constantly for fast
            // CDP commands where stop() is called right after spawn. Without this
            // top check the lost notify means we wait the full budget before
            // noticing, and stop()'s join() blocks for that whole time.
            if *cancelled {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                fired_c.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(stale) = stale.as_ref() {
                    stale.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                watchdog_terminate(&handle);
                return;
            }
            let (guard, _) = cvar.wait_timeout(cancelled, remaining).unwrap();
            cancelled = guard;
            if *cancelled {
                return;
            }
        }
    });
    WatchdogToken { pair, join: Some(join), fired, sampler_stop }
}

impl Drop for WatchdogToken {
    /// Cancellation safety for armed watchdogs. Async callers (the session
    /// idle pump, settle futures) can be dropped between `arm_watchdog` and
    /// `disarm_watchdog` — `tokio::select!` preempts the pump the moment a
    /// command arrives. Without this the orphaned thread later fires
    /// `terminate_execution()` into whatever runs next and bricks the
    /// isolate (no `cancel_terminate_execution` ever follows; measured as
    /// every subsequent eval failing with "Uncaught Error: execution
    /// terminated"). Dropping the token cancels the thread instead: it
    /// sleeps in `wait_timeout` while holding the mutex, so once we acquire
    /// the lock the thread can only wake, see the cancel flag and exit — it
    /// cannot have terminated in between. The remaining sliver (it fired
    /// while we were blocked on the lock) is healed by the
    /// retry-on-terminated path in `evaluate`.
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

impl WatchdogToken {
    /// Stop the watchdog. Returns true if it had already fired (terminated the
    /// isolate). The caller must then clear the termination flag via
    /// [`JsRuntime::cancel_termination`] before the next eval.
    pub fn stop(mut self) -> bool {
        self.cancel_and_join();
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Shared cancel path for Drop and stop(): flip the cancel flag, join the
    /// watchdog thread, then halt the stack-dump sampler. Joining first
    /// guarantees the sampler stops AFTER the watchdog can no longer fire, so
    /// its last in-flight request can at worst dump a healthy stack once.
    fn cancel_and_join(&mut self) {
        {
            let (lock, cvar) = &*self.pair;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if let Some(stop) = self.sampler_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether this watchdog has already terminated the isolate, without
    /// stopping it. Lets a pump loop poll for termination between slices
    /// while keeping the token alive for the final `stop()`.
    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}
