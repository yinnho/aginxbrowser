//! # async_glue — 手写 async op 胶水（我们的 Vm 宿主侧）
//!
//! 批 10c 的 diting 半边：真四 async op（`op_fetch_url`、`op_sleep`、
//! `op_ws_open`、`op_ws_next_message`）在我们自己的宿主（diting_runtime
//! 的 Vm + OpSet）上跑时，不走 `#[op2]` 宏——宏产的是 deno 形状；这里
//! 逐句照 `diting_runtime/tests/async_glue.rs` 的模板手写：
//!
//! - raw wrapper 收 `(promise_id, 用户参...)`——JS 异步桩
//!   （`__diting_wrap_async`，install_onto 装的）已在环表里分配 promise
//!   并前插 id（deno_core `setUpAsyncStub` 法），wrapper 里零 promise
//!   机械、零 scope 味道难题；
//! - JS 参先解码、state 后解码（decode-throw 永不持有 RefCell）；
//! - 身体 future 经 [`spawn_async_op`] 灌进 OpDriver，完成序即投递序。
//!
//! 10d 翻转后本模块是这四个 op 的唯一宿主（`ops::our_decls()` 把
//! `all_decls()` 并进同步表一起装进 `Deno.core.ops`）——`#[op2]` 桩已死。

use diting_runtime::op_glue::{throw_error_one_byte_info, to_f64_option, to_i32_option, to_string};
use diting_runtime::op_reg::{OpCtx, OpDecl, SlowFn, SlowFnImpl};
use diting_runtime::spawn_async_op;

use super::ops::SharedState;

fn async_decl(
    name: &'static str,
    arg_count: u8,
    slow_fn: SlowFn,
    slow_function_impl: SlowFnImpl,
) -> OpDecl {
    let mut decl = OpDecl::new(name, arg_count, slow_fn, slow_function_impl);
    // install_onto wraps is_async decls in the JS stub — the JS caller sees
    // the stub (promise out), the raw wrapper here sees (promise_id, args...).
    decl.is_async = true;
    decl
}

/// Decode the OpCtx from the wrapper's `args.data()` External — the same
/// pointer install_onto pinned when it built the function.
///
/// # Safety
/// `data` must be the External install_onto built for this op (OpSet boxes
/// live until isolate disposal).
unsafe fn opctx_of<'a>(args: &'a v8::FunctionCallbackArguments<'_>) -> &'a OpCtx {
    &*(v8::Local::<v8::External>::cast_unchecked(args.data()).value() as *const OpCtx)
}

// ── op_sleep ──────────────────────────────────────────────────────────

/// `op_sleep(millis)` — the JS shape bootstrap keeps calling after the
/// assembly swap. Resolves with `""` where deno resolves `undefined`; the
/// only call sites arm `.then()` and never read the value.
fn op_sleep_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let millis = match to_f64_option(&args.get(1)) {
        Some(v) => v as u64,
        None => {
            throw_error_one_byte_info(info, "expected f64");
            return 1;
        }
    };
    let fut = async move {
        super::ops::op_sleep_body(millis).await;
        Ok(String::new())
    };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn op_sleep_entry(info: *const v8::FunctionCallbackInfo) {
    op_sleep_impl(info);
}

pub fn op_sleep_decl() -> OpDecl {
    async_decl("op_sleep", 1, op_sleep_entry as _, op_sleep_impl)
}

// ── op_ws_open ────────────────────────────────────────────────────────

/// `op_ws_open(url, protocols_json)` — resolves the `{"id":N}` JSON, or the
/// `{"error":...}` JSON when the open must fail (the JS layer surfaces both
/// the same way, promise always resolves).
fn op_ws_open_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    // String decode needs a mutable isolate view — the CallbackScope deref
    // chain (the macro's law for #[string] args).
    let mut scope_store = unsafe { v8::CallbackScope::new(info) };
    let scope = unsafe { std::pin::Pin::new_unchecked(&mut scope_store) };
    let mut scope = scope.init();
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let url = to_string(&mut scope, &args.get(1));
    let protocols_json = to_string(&mut scope, &args.get(2));
    // State decodes after all JS args (the macro's law).
    let opctx = unsafe { opctx_of(&args) };
    let gs_cell = opctx.state.borrow().borrow::<SharedState>().clone();
    let fut = async move { Ok(super::ws::op_ws_open_body(gs_cell, url, protocols_json).await) };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn op_ws_open_entry(info: *const v8::FunctionCallbackInfo) {
    op_ws_open_impl(info);
}

pub fn op_ws_open_decl() -> OpDecl {
    async_decl("op_ws_open", 2, op_ws_open_entry as _, op_ws_open_impl)
}

// ── op_ws_next_message ────────────────────────────────────────────────

/// `op_ws_next_message(id)` — resolves the next wire→JS event JSON; a dead
/// id resolves `{"kind":"close","code":1006,"reason":""}`.
fn op_ws_next_message_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let id = match to_f64_option(&args.get(1)) {
        Some(v) => v as u64,
        None => {
            throw_error_one_byte_info(info, "expected f64");
            return 1;
        }
    };
    let opctx = unsafe { opctx_of(&args) };
    let gs_cell = opctx.state.borrow().borrow::<SharedState>().clone();
    let fut = async move { Ok(super::ws::op_ws_next_message_body(gs_cell, id).await) };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn op_ws_next_message_entry(info: *const v8::FunctionCallbackInfo) {
    op_ws_next_message_impl(info);
}

pub fn op_ws_next_message_decl() -> OpDecl {
    async_decl(
        "op_ws_next_message",
        1,
        op_ws_next_message_entry as _,
        op_ws_next_message_impl,
    )
}

// ── op_fetch_url ──────────────────────────────────────────────────────

/// `op_fetch_url(url, method, headers_json, body, origin, mode,
/// credentials, referrer)` — resolves the settlement JSON payload; rejects
/// with the error message when the walk fails (the fetch shim surfaces
/// rejects as TypeError, same as the deno host did).
fn op_fetch_url_impl(info: *const v8::FunctionCallbackInfo) -> usize {
    let info = unsafe { &*info };
    let parts: v8::FunctionCallbackInfoParts<'_> = info.get_parts();
    let args = v8::FunctionCallbackArguments::from_function_callback_info_parts(info, &parts);
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr(&parts.isolate) };
    let mut scope_store = unsafe { v8::CallbackScope::new(info) };
    let scope = unsafe { std::pin::Pin::new_unchecked(&mut scope_store) };
    let mut scope = scope.init();
    let promise_id = to_i32_option(&args.get(0)).unwrap_or_default();
    let url = to_string(&mut scope, &args.get(1));
    let method = to_string(&mut scope, &args.get(2));
    let headers_json = to_string(&mut scope, &args.get(3));
    let body = to_string(&mut scope, &args.get(4));
    let origin = to_string(&mut scope, &args.get(5));
    let mode = to_string(&mut scope, &args.get(6));
    let credentials = to_string(&mut scope, &args.get(7));
    // "policy\0init\0settle_key" bundle (see op_fetch_url_body).
    let referrer = to_string(&mut scope, &args.get(8));
    let opctx = unsafe { opctx_of(&args) };
    let state = opctx.state.clone();
    let fut = async move {
        super::ops::op_fetch_url_body(
            state, url, method, headers_json, body, origin, mode, credentials, referrer,
        )
        .await
        .map_err(|e| e.to_string())
    };
    spawn_async_op(isolate, promise_id, fut);
    0
}

unsafe extern "C" fn op_fetch_url_entry(info: *const v8::FunctionCallbackInfo) {
    op_fetch_url_impl(info);
}

pub fn op_fetch_url_decl() -> OpDecl {
    async_decl(
        "op_fetch_url",
        8,
        op_fetch_url_entry as _,
        op_fetch_url_impl,
    )
}

// Re-exported for the 10d assembly: the full async-four decl list our host
// installs alongside the sync ops (ops::our_decls()).
pub fn all_decls() -> Vec<OpDecl> {
    vec![
        op_fetch_url_decl(),
        op_sleep_decl(),
        op_ws_open_decl(),
        op_ws_next_message_decl(),
    ]
}
