//! 批 10c 钉测：手写 async op 胶水机制端到端（裸 Vm，零 deno_core）。
//!
//! 这里的测试 op 与 diting 侧真四 op 的胶水（`diting_js::async_glue`）
//! 逐句同构：raw wrapper 收 `(promise_id, 用户参...)`——JS 异步桩
//! （`__diting_wrap_async`，install_onto 装的）已在环表里分配 promise 并
//! 前插 id（deno_core `setUpAsyncStub` 法）——解码后把身体 future 经
//! [`spawn_async_op`] 灌进 OpDriver。本二进制钉机制面；带真身体（tokio
//! 定时器、WS 注册表）的钉测在 diting 的独立测试二进制——rusty_v8 的
//! platform 状态机二次初始化必炸，而 diting 的 lib 单测二进制里全是
//! deno JsRuntime，两个世界不能同进程。

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use diting_runtime::op_glue::{throw_error_one_byte_info, to_f64_option, to_i32_option, to_string};
use diting_runtime::op_reg::{OpCtx, OpDecl};
use diting_runtime::state::OpState;
use diting_runtime::vm::Vm;
use diting_runtime::{AsyncOpOutcome, spawn_async_op};

/// The state face the test op reads through the OpSet's shared cell —
/// pins that the glue's OUR-OpCtx decode reaches the same OpState the
/// host seeded.
struct Prefix(String);

/// Pending until the Nth poll, then done — the deterministic stand-in
/// for a tokio timer.
struct AfterNPolls {
    n: u32,
    outcome: Option<AsyncOpOutcome>,
}

impl AfterNPolls {
    fn new(n: u32, outcome: AsyncOpOutcome) -> Self {
        Self {
            n,
            outcome: Some(outcome),
        }
    }
}

impl Future for AfterNPolls {
    type Output = AsyncOpOutcome;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.n == 0 {
            Poll::Ready(this.outcome.take().expect("polled after completion"))
        } else {
            this.n -= 1;
            Poll::Pending
        }
    }
}

fn async_decl(
    name: &'static str,
    arg_count: u8,
    slow_fn: diting_runtime::op_reg::SlowFn,
    slow_function_impl: diting_runtime::op_reg::SlowFnImpl,
) -> OpDecl {
    let mut decl = OpDecl::new(name, arg_count, slow_fn, slow_function_impl);
    // install_onto wraps is_async decls in the JS stub — the caller of
    // the op sees the stub, the raw wrapper sees (promise_id, args...).
    decl.is_async = true;
    decl
}

/// `__t_async_upper(text, polls)` — resolves `P:` + text after `polls`
/// gated drive passes. User args sit at 1.. behind the stub's promise
/// id; the polls arg is the op2 `#[number]` slow-path decode
/// (`to_f64_option` then the saturating `as u64`).
fn __t_async_upper_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    // String decode needs a mutable isolate view — the CallbackScope
    // deref chain (the macro's law for #[string] args).
    let mut scope_store = unsafe { v8::CallbackScope::new(info) };
    let scope = unsafe { std::pin::Pin::new_unchecked(&mut scope_store) };
    let mut scope = scope.init();
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let text = to_string(&mut scope, &args.get(1));
    let polls = match to_f64_option(&args.get(2)) {
        Some(v) => v as u64,
        None => {
            throw_error_one_byte_info(info, "expected f64");
            return 1;
        }
    };
    // State decodes after all JS args (the macro's law): decode-throws
    // never hold the RefCell.
    let opctx: &OpCtx = unsafe {
        &*(v8::Local::<v8::External>::cast_unchecked(args.data()).value() as *const OpCtx)
    };
    let prefix = opctx.state.borrow().borrow::<Prefix>().0.clone();
    let fut = async move { AfterNPolls::new(polls as u32, Ok(format!("{prefix}{text}"))).await };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn __t_async_upper_entry(info: *const v8::FunctionCallbackInfo) {
    __t_async_upper_impl(info);
}

fn __t_async_upper() -> OpDecl {
    async_decl(
        "__t_async_upper",
        2,
        __t_async_upper_entry as _,
        __t_async_upper_impl,
    )
}

/// Rejects "boom" after two gated passes — the Err arm of the outcome.
fn __t_async_reject_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let fut = async move { AfterNPolls::new(2, Err("boom".into())).await };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn __t_async_reject_entry(info: *const v8::FunctionCallbackInfo) {
    __t_async_reject_impl(info);
}

fn __t_async_reject() -> OpDecl {
    async_decl(
        "__t_async_reject",
        0,
        __t_async_reject_entry as _,
        __t_async_reject_impl,
    )
}

fn vm_with_async_ops() -> (Vm, Rc<std::cell::RefCell<OpState>>) {
    let mut vm = Vm::new();
    let state = vm.install_ops(vec![__t_async_upper(), __t_async_reject()]);
    state.borrow_mut().put(Prefix("P:".into()));
    (vm, state)
}

fn pump(vm: &mut Vm, rounds: usize) {
    for _ in 0..rounds {
        vm.poll().expect("vm.poll drives and delivers");
    }
}

#[test]
fn async_op_resolves_through_ring_table_with_state() {
    let (mut vm, _state) = vm_with_async_ops();
    vm.eval_string(
        "globalThis.got = ''; \
         __t_async_upper('x', 1).then(function (v) { globalThis.got = 'R:' + v; });",
    )
    .expect("call sites arm");
    assert_eq!(vm.eval_string("globalThis.got").unwrap(), "");
    // The op call returned a promise (then armed), not undefined.
    assert_eq!(vm.pending_ops(), 1);
    pump(&mut vm, 4);
    // The body read the OpSet cell through OUR OpCtx decode (prefix), the
    // promise resolved through the ring table, microtasks drained.
    assert_eq!(vm.eval_string("globalThis.got").unwrap(), "R:P:x");
    // The ring entry retired on settlement.
    assert_eq!(vm.eval_string("__diting_tableCounts()").unwrap(), "0,0");
    assert_eq!(vm.pending_ops(), 0);
}

#[test]
fn async_op_rejects_with_error_message() {
    let (mut vm, _state) = vm_with_async_ops();
    vm.eval_string(
        "globalThis.got = ''; \
         __t_async_reject().then(\
             function () { globalThis.got = 'no'; }, \
             function (e) { globalThis.got = 'E:' + e.message; });",
    )
    .expect("call sites arm");
    pump(&mut vm, 4);
    assert_eq!(vm.eval_string("globalThis.got").unwrap(), "E:boom");
}

#[test]
fn two_in_flight_ops_settle_in_completion_order() {
    let (mut vm, _state) = vm_with_async_ops();
    // The slow one (2 gated passes) is armed FIRST; the fast one (0) lands
    // after. Completion order is the delivery order — fast settles on the
    // first drive pass, slow on the third.
    vm.eval_string(
        "globalThis.order = ''; \
         __t_async_upper('slow', 2).then(function () { globalThis.order += 'slow,'; }); \
         __t_async_upper('fast', 0).then(function () { globalThis.order += 'fast'; });",
    )
    .expect("call sites arm");
    pump(&mut vm, 5);
    assert_eq!(vm.eval_string("globalThis.order").unwrap(), "fastslow,");
}

#[test]
fn non_number_polls_arg_throws_before_promise_exists() {
    let (mut vm, _state) = vm_with_async_ops();
    // The op2 slow-path decode law: a non-number where `#[number]` u64 is
    // expected throws "expected f64" at the call site — no driver entry,
    // and the stub's promise hangs (never settled; the table entry stays
    // until something settles it — count it, don't deliver it).
    vm.eval_string(
        "globalThis.got = ''; \
         try { __t_async_upper('x', 'nope'); } catch (e) { globalThis.got = 'T:' + e.message; }",
    )
    .expect("call sites arm");
    assert_eq!(vm.eval_string("globalThis.got").unwrap(), "T:expected f64");
    assert_eq!(vm.pending_ops(), 0);
    // The stub registered its promise before the raw fn threw — the ring
    // entry is orphaned, exactly like deno's (its promise table leaks the
    // slot too; GC owns the JS side).
    assert_eq!(vm.eval_string("__diting_tableCounts()").unwrap(), "1,0");
}
