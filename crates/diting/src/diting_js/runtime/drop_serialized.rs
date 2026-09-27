pub(super) static ISOLATE_CONSTRUCT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// #108: the inner runtime wrapped so its ISOLATE DISPOSAL serializes with
/// isolate construction under `ISOLATE_CONSTRUCT_LOCK`. Construction was
/// already serialized (upstream obscura #430 lineage); disposal is the one
/// remaining seam where V8 15.0's process-wide JSDispatchTable sees alloc
/// (another isolate's snapshot deserialization, under the lock) race free
/// (this isolate's teardown, previously unlocked). The #108 failure shape —
/// `typeof Date.now` healthy inside a timer callback while the CALL throws
/// "not a function", dev-only, unreproducible on demand — is a call-path
/// (dispatch-entry) failure with a healthy object, exactly what a torn
/// table entry produces; the stress reproducers (tests: `date_now_timer_*`)
/// could not trigger it in ~10k construct/drop cycles, so this is
/// defense-in-depth closing the last uncovered cross-isolate touch point,
/// not a demonstrated fix.
///
/// `ManuallyDrop` is the std drop-exactly-once idiom: `Drop::drop` runs
/// with the lock held and destroys the inner runtime explicitly; the
/// compiler's own field drop is suppressed, so teardown happens exactly
/// once, inside the guard.
pub(super) struct SerializedDropRuntime(
    pub(super) std::mem::ManuallyDrop<deno_core::JsRuntime>,
);

impl std::ops::Deref for SerializedDropRuntime {
    type Target = deno_core::JsRuntime;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SerializedDropRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for SerializedDropRuntime {
    fn drop(&mut self) {
        let _guard = ISOLATE_CONSTRUCT_LOCK.lock().unwrap();
        // SAFETY: `self` is being dropped and nothing touches the field
        // afterwards; `ManuallyDrop` suppressed the automatic drop, so this
        // read-then-drop runs the isolate teardown exactly once — here,
        // under the construction lock rather than after it.
        unsafe { std::mem::drop(std::ptr::read(&*self.0)) }
    }
}
