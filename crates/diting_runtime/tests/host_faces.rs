//! 批 10d-2 钉测：Vm 的宿主脸——`execute_script`（带名 origin、值以
//! Global 存活）、`with_scope`（deno_core::scope! 形状的换值面）、
//! `isolate_handle`（watchdog 家族的跨线程 terminate/cancel 手柄）。
//! 这些是 10d 装配翻转时 runtime.rs 需要的全部新面，先在裸 Vm 上钉死。

use diting_runtime::vm::{LoopError, Vm};

#[test]
fn execute_script_returns_the_value_as_a_global() {
    let mut vm = Vm::new();
    let global = vm.execute_script("t", "6 * 7").expect("script runs");
    let n = vm.with_scope(|scope| {
        let local = v8::Local::new(scope, &global);
        local.number_value(scope)
    });
    assert_eq!(n, Some(42.0));
}

#[test]
fn execute_script_surfaces_the_name_in_error_stacks() {
    let mut vm = Vm::new();
    let err = vm
        .execute_script("<eval>", "nope()")
        .expect_err("nope is not defined");
    match &err {
        LoopError::TickThrew(msg) => {
            assert!(msg.contains("nope is not defined"), "message was {msg}");
        }
        other => panic!("expected TickThrew, got {other:?}"),
    }
    // The origin name rides the thrown error's stack (via with_scope, the
    // host's conversion channel — message object carries the resource).
    let has_name = vm.with_scope(|scope| vm_scope_trycatch_stack_has(scope, "<eval>"));
    assert!(has_name, "script name should surface via the scope face");
}

/// Re-run the failing script under a TryCatch and check the message's
/// source name — the deno execute_script behavior the host's error
/// reporting leans on.
fn vm_scope_trycatch_stack_has(scope: &mut v8::PinScope<'_, '_>, needle: &str) -> bool {
    v8::tc_scope!(let tc, scope);
    let src = v8::String::new(tc, "nope()").unwrap();
    let name = v8::String::new(tc, "<eval>").unwrap();
    let origin = v8::ScriptOrigin::new(
        tc,
        name.into(),
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
    if let Some(script) = v8::Script::compile(tc, src, Some(&origin)) {
        let _ = script.run(tc);
    }
    match tc.message() {
        Some(m) => m
            .get_script_resource_name(tc)
            .map(|r| r.to_rust_string_lossy(tc).contains(needle))
            .unwrap_or(false),
        None => false,
    }
}

#[test]
fn with_scope_sees_the_context_globals() {
    let mut vm = Vm::new();
    vm.eval_string("globalThis.__probe = 'ctx'").unwrap();
    let got = vm.with_scope(|scope| {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__probe").unwrap();
        global
            .get(scope, key.into())
            .map(|v| v.to_rust_string_lossy(scope))
            .unwrap_or_default()
    });
    assert_eq!(got, "ctx");
}

#[test]
fn isolate_handle_terminates_and_revives_the_stale_shape() {
    let mut vm = Vm::new();
    // The #50 shape: the handle fires while NO JS is on the stack — the
    // termination lands pending on an idle isolate, and the next entry
    // dies. Cancel from the owning thread clears it.
    let handle = vm.isolate_handle();
    handle.terminate_execution();
    let err = vm
        .execute_script("t", "1 + 1")
        .expect_err("pending termination kills the next entry");
    assert!(matches!(err, LoopError::Terminated));
    vm.revive();
    let revived = vm.execute_script("t", "'ok'").expect("revived entry runs");
    let s = vm.with_scope(|scope| {
        let local = v8::Local::new(scope, &revived);
        local.to_rust_string_lossy(scope)
    });
    assert_eq!(s, "ok");
    // The handle stays valid after use (Arc-clone shape, watchdog holds one).
    handle.cancel_terminate_execution();
}

// ── poll_event_loop：reactor 集成脸 ────────────────────────────────────

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use diting_runtime::{LoopActivity, Triplet};

/// A test waker that records the wake in a flag (the "reactor fired"
/// stand-in).
struct FlagWake(Arc<AtomicBool>);
impl std::task::Wake for FlagWake {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

fn flag_waker() -> (std::task::Waker, Arc<AtomicBool>) {
    let flag = Arc::new(AtomicBool::new(false));
    (
        std::task::Waker::from(Arc::new(FlagWake(flag.clone()))),
        flag,
    )
}

/// An op future that parks holding the caller's waker until an external
/// flag flips and the held waker fires — the exact shape of a tokio-timer
/// or network op body under the reactor host.
struct ParkOnWaker {
    go: Arc<AtomicBool>,
    held: Arc<Mutex<Option<std::task::Waker>>>,
    triplet: Option<Triplet>,
}
impl Future for ParkOnWaker {
    type Output = Triplet;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Triplet> {
        let this = self.get_mut();
        if this.go.load(Ordering::Acquire) {
            Poll::Ready(this.triplet.take().expect("polled after completion"))
        } else {
            *this.held.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[test]
fn poll_event_loop_passes_the_caller_waker_to_op_bodies() {
    let mut vm = Vm::new();
    let (waker, woke) = flag_waker();
    let go = Arc::new(AtomicBool::new(false));
    let held: Arc<Mutex<Option<std::task::Waker>>> = Arc::new(Mutex::new(None));
    vm.spawn_op(ParkOnWaker {
        go: go.clone(),
        held: held.clone(),
        triplet: Some(Triplet {
            promise_id: 0,
            ok: true,
            payload: String::new(),
        }),
    });

    // First pass: the body parks, holding OUR waker — not the driver's
    // advisory flag, which a reactor host never reads.
    let mut cx = Context::from_waker(&waker);
    let a = vm.poll_event_loop(&mut cx).expect("first pass");
    assert!(!a.idle(), "one op in flight");
    assert!(!a.delivered);
    assert!(
        held.lock().unwrap().is_some(),
        "the body held the caller's waker"
    );

    // The reactor fires: flag + held waker. The wake lands on OUR flag
    // (that IS the host being woken); the next pass completes + delivers.
    go.store(true, Ordering::Release);
    held.lock().unwrap().take().unwrap().wake();
    assert!(woke.load(Ordering::Acquire), "the held waker was ours");
    let b = vm
        .poll_event_loop(&mut Context::from_waker(&waker))
        .expect("second pass");
    assert_eq!(
        b,
        LoopActivity {
            delivered: true,
            pending_ops: 0,
            pending_modules: false,
        },
        "completed, delivered, and idle — one wake, one pass"
    );
}

#[test]
fn poll_event_loop_reports_idle_with_nothing_in_flight() {
    let mut vm = Vm::new();
    vm.eval_string("globalThis.__diting_tableCounts()").unwrap();
    let (waker, _) = flag_waker();
    let mut cx = Context::from_waker(&waker);
    let a = vm.poll_event_loop(&mut cx).expect("empty pass");
    assert!(a.idle(), "no ops, no modules — {a:?}");
    assert!(!a.delivered);
}
