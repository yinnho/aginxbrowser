//! V8 foreground tasks — the custom-platform face of the loop.
//!
//! deno_core 0.411 `runtime/setup.rs` law (MIT, 2018-2026 Deno authors),
//! learned not forked: V8 finishes asynchronous engine work (wasm async
//! compilation is the canonical one) by posting a FOREGROUND task — code
//! that must run on the isolate's thread. The default platform parks those
//! in a runner the embedder never pumps, and nothing wakes the loop when
//! one lands, so an `await WebAssembly.instantiate(bytes)` promise hangs
//! forever on a bare Vm. deno_core installs a custom platform whose posts
//! land in a per-isolate registry AND wake the loop's registered waker; the
//! event loop drains the queue at the top of every pass (its "Pre-phase:
//! drain foreground tasks"). Same law here, minus its tokio dependency:
//! immediate posts queue + wake; delayed posts enter the same queue with a
//! due-time and run on the first pass at/after it (divergence note on
//! [`DitingPlatformImpl::post_delayed_task`]).
//!
//! May be called from ANY thread (V8 background workers) — hence the
//! Mutex-everywhere shape; [`std::task::Waker`] is Send+Sync so the stored
//! clone can be woken from the posting thread directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Waker;
use std::time::Instant;

/// One queued post: the task, and when it may run (`None` = immediately).
/// Delayed posts carry an `Instant` computed at post time — V8's clock base
/// differs from `std::time`'s by an unknowable offset, which only shifts
/// runtimes by that constant; the pass cadence governs precision anyway.
struct QueuedTask {
    task: v8::Task,
    due: Option<Instant>,
}

/// One isolate's foreground-task queue + its loop waker. Registered in the
/// global [`REGISTRY`] keyed by the raw isolate pointer.
pub(crate) struct ForegroundEntry {
    tasks: Mutex<Vec<QueuedTask>>,
    waker: Mutex<Option<Waker>>,
}

static REGISTRY: OnceLock<Mutex<HashMap<usize, Arc<ForegroundEntry>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<usize, Arc<ForegroundEntry>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

struct DitingPlatformImpl;

impl v8::PlatformImpl for DitingPlatformImpl {
    fn post_task(&self, isolate_ptr: *mut std::ffi::c_void, task: v8::Task) {
        queue_task(isolate_ptr as usize, task);
    }

    fn post_non_nestable_task(&self, isolate_ptr: *mut std::ffi::c_void, task: v8::Task) {
        queue_task(isolate_ptr as usize, task);
    }

    /// Divergence from deno_core (which spawns each delayed post on tokio
    /// with a real timer, aborting when no handle exists): this crate owns
    /// no reactor dependency — delayed posts enter the SAME queue with a
    /// due-time, and the loop runs them on the first pass at/after it.
    /// They never run inline: V8 posts them from inside V8 (heap
    /// bookkeeping is the canonical poster), and re-entrant execution there
    /// is a stack overflow (pinned by the ring-overflow test's first run
    /// of this file — delayed task ran mid-GC, re-posted, recursed).
    fn post_delayed_task(&self, isolate_ptr: *mut std::ffi::c_void, task: v8::Task, delay_in_seconds: f64) {
        queue_delayed(isolate_ptr as usize, task, delay_in_seconds);
    }

    fn post_non_nestable_delayed_task(
        &self,
        isolate_ptr: *mut std::ffi::c_void,
        task: v8::Task,
        delay_in_seconds: f64,
    ) {
        queue_delayed(isolate_ptr as usize, task, delay_in_seconds);
    }

    /// Built with idle_task_support=false — V8 never posts idle tasks here.
    /// Dropping the task destroys it unexecuted, the v8-documented semantic.
    fn post_idle_task(&self, _isolate_ptr: *mut std::ffi::c_void, task: v8::IdleTask) {
        drop(task);
    }
}

fn queue_task(key: usize, task: v8::Task) {
    queue(QueuedTask { task, due: None }, key);
}

fn queue_delayed(key: usize, task: v8::Task, delay_in_seconds: f64) {
    let due = Instant::now() + std::time::Duration::from_secs_f64(delay_in_seconds.max(0.0));
    queue(QueuedTask { task, due: Some(due) }, key);
}

fn queue(qt: QueuedTask, key: usize) {
    let entry = registry().lock().unwrap().get(&key).cloned();
    let Some(entry) = entry else {
        // deno_core's law, verbatim shape: an unregistered isolate's posts
        // are DROPPED (`else { return }` in its queue_task). Never run
        // inline — V8 posts from inside allocation-observer steps and GC
        // epilogues, where re-entrant execution trips
        // `Check failed: !allocation_counter().IsStepInProgress()` (this
        // file's first run did exactly that: MinorGCJob task posted from
        // ScheduleMinorGCTaskObserver::Step, run inline, V8 SIGTRAP-fatal).
        // The one unregistered isolate is the snapshot creator: its posts
        // are heap bookkeeping, and the bake needs none of them to finish.
        drop(qt);
        return;
    };
    entry.tasks.lock().unwrap().push(qt);
    let waker = entry.waker.lock().unwrap().clone();
    if let Some(waker) = waker.as_ref() {
        waker.wake_by_ref();
    }
}

/// The loop's platform — what [`crate::vm`]'s init installs instead of the
/// default platform. Thread pool capped at 4 (Node's posture, deno's
/// number): background compilation rarely wants more, and every thread is
/// resident memory.
pub fn platform() -> v8::SharedRef<v8::Platform> {
    let thread_pool_size = std::cmp::min(
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4),
        4,
    );
    // unprotected=true unconditionally (deno's law is tests-or-feature; we
    // go one further): our Vms are NEVER confined to the init thread —
    // prod isolates live on tokio worker threads, tests on cargo test
    // threads. Thread-isolated allocations would be wrong everywhere we
    // run. (No-op on macOS; correctness on Linux.)
    v8::new_custom_platform(thread_pool_size, false, true, DitingPlatformImpl).make_shared()
}

/// A live Vm registers at build and deregisters in Drop — the pointer key is
/// only unique while the isolate lives (a dropped isolate's address can be
/// reused by the next one).
pub(crate) fn register(isolate_ptr: usize) -> Arc<ForegroundEntry> {
    let entry = Arc::new(ForegroundEntry {
        tasks: Mutex::new(Vec::new()),
        waker: Mutex::new(None),
    });
    registry().lock().unwrap().insert(isolate_ptr, entry.clone());
    entry
}

pub(crate) fn deregister(isolate_ptr: usize) {
    registry().lock().unwrap().remove(&isolate_ptr);
}

/// Fresh waker every pass, like the op driver's loop_waker cell.
pub(crate) fn set_waker(entry: &ForegroundEntry, waker: &Waker) {
    *entry.waker.lock().unwrap() = Some(waker.clone());
}

/// Run every DUE task now, re-queueing the not-yet-due tail. MUST be called
/// with the isolate entered — foreground tasks touch V8 (they resolve
/// promises, run microtask-hungry callbacks). Returns how many ran.
pub(crate) fn drain(entry: &ForegroundEntry) -> usize {
    let tasks = std::mem::take(&mut *entry.tasks.lock().unwrap());
    let now = Instant::now();
    let (due, later): (Vec<_>, Vec<_>) = tasks.into_iter().partition(|qt| qt.due.is_none_or(|d| d <= now));
    *entry.tasks.lock().unwrap() = later;
    let count = due.len();
    for qt in due {
        qt.task.run();
    }
    count
}

/// Queued-and-DUE tasks — part of the loop's not-idle signal. A not-yet-due
/// task deliberately reads idle: nothing can wake the loop at its deadline
/// (no reactor here), and V8's delayed posts are heap bookkeeping, never
/// page-visible semantics — the next pass picks them up.
pub(crate) fn has_pending(entry: &ForegroundEntry) -> bool {
    let now = Instant::now();
    entry
        .tasks
        .lock()
        .unwrap()
        .iter()
        .any(|qt| qt.due.is_none_or(|d| d <= now))
}
