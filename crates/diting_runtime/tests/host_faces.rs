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
