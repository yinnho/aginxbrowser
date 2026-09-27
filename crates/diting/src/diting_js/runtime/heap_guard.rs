use super::JsRuntime;
use deno_core::v8::IsolateHandle;

/// How much the near-heap-limit callback raises the limit so V8 can unwind
/// the terminated script instead of aborting the process.
pub(super) const HEAP_LIMIT_RECOVERY_HEADROOM_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct HeapLimitState {
    tripped: std::sync::atomic::AtomicBool,
    restore_limit: std::sync::atomic::AtomicUsize,
}

/// V8's default response to hitting the heap limit is to abort the whole
/// process — with many sessions in one server, one page's allocation loop
/// would kill every session. The callback terminates the current script
/// instead and lends the isolate just enough headroom to unwind.
pub(super) fn install_heap_limit_guard(
    runtime: &mut deno_core::JsRuntime,
    isolate_handle: IsolateHandle,
    state: std::sync::Arc<HeapLimitState>,
) {
    runtime.add_near_heap_limit_callback(move |current_limit, _initial_limit| {
        let _ = state.restore_limit.compare_exchange(
            0,
            current_limit,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        state.tripped.store(true, std::sync::atomic::Ordering::SeqCst);
        // #50 probe: the second captured repro showed tick_fn terminating
        // with NO watchdog fire anywhere near it — this guard is the only
        // other silent terminate_execution() call site. Make it visible.
        let t_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        eprintln!(
            "[heapguard] near-heap-limit tripped at {} bytes (initial {}) — terminating t={t_ms}",
            current_limit, _initial_limit
        );
        isolate_handle.terminate_execution();
        current_limit.saturating_add(HEAP_LIMIT_RECOVERY_HEADROOM_BYTES)
    });
}


impl JsRuntime {
    /// If the heap-limit guard terminated the last script, recover the
    /// isolate before new JS runs: cancel the termination and restore the
    /// real heap limit (the callback had inflated it to let V8 unwind).
    pub(super) fn recover_heap_limit(&mut self) -> bool {
        if !self
            .heap_limit_state
            .tripped
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.runtime.v8_isolate().cancel_terminate_execution();
        let restore_limit = self
            .heap_limit_state
            .restore_limit
            .swap(0, std::sync::atomic::Ordering::SeqCst);
        self.runtime.remove_near_heap_limit_callback(restore_limit);
        install_heap_limit_guard(
            &mut self.runtime,
            self.isolate_handle.clone(),
            self.heap_limit_state.clone(),
        );
        tracing::warn!("V8 heap limit reached: terminated the current JavaScript task");
        true
    }
}
