//! 批 10c 钉测（diting 半边）：真三 async op 的手写胶水在裸 Vm 上闭环。
//!
//! 独立测试二进制——rusty_v8 的 platform 状态机二次初始化必炸，而 diting
//! 的 lib 单测二进制里全是 deno JsRuntime，我们的裸 Vm 只能自己一个进程
//! （diting_runtime::tests 的机制面钉测同一条法）。
//!
//! 全部用确定路径：真 tokio 定时器（current_thread rt + enter guard）、
//! URL 解析失败（零网络）、死 socket id（注册表 miss）。零外联。

use std::cell::RefCell;
use std::rc::Rc;

use diting::diting_js::async_glue;
use diting::diting_js::ops::{JsState, SharedState};
use diting_runtime::vm::Vm;

fn bare_vm() -> (Vm, Rc<RefCell<diting_runtime::state::OpState>>) {
    let mut vm = Vm::new();
    let state = vm.install_ops(async_glue::all_decls());
    state
        .borrow_mut()
        .put(Rc::new(RefCell::new(JsState::new())) as SharedState);
    (vm, state)
}

fn pump(vm: &mut Vm, rounds: usize) {
    for _ in 0..rounds {
        vm.poll().expect("vm.poll drives and delivers");
    }
}

/// Real-timer drive: the current_thread rt's timer driver only dispatches
/// expired deadlines while parked, and a registered `Sleep` never re-reads
/// the clock on plain re-poll — so between Vm passes we let tokio walk time
/// (`block_on(sleep(2ms))` parks the thread on the driver). This is the
/// drive shape the 10d runtime loop will use for op bodies between
/// event-loop passes.
fn pump_until(rt: &tokio::runtime::Runtime, vm: &mut Vm, probe: &str, rounds: usize) -> String {
    for _ in 0..rounds {
        vm.poll().expect("vm.poll drives and delivers");
        let got = vm.eval_string(probe).unwrap_or_default();
        if !got.is_empty() {
            return got;
        }
        rt.block_on(tokio::time::sleep(std::time::Duration::from_millis(2)));
    }
    vm.eval_string(probe).unwrap_or_default()
}

#[test]
fn op_sleep_resolves_after_real_timer() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test rt builds");
    let _guard = rt.enter();
    let (mut vm, _state) = bare_vm();
    vm.eval_string(
        "globalThis.got = ''; \
         op_sleep(5).then(function () { globalThis.got = 'slept'; });",
    )
    .expect("call site arms");
    assert_eq!(vm.eval_string("globalThis.got").unwrap(), "");
    assert_eq!(vm.pending_ops(), 1);
    // 5ms timer, 2ms pump cadence, generous budget for CI scheduling.
    let got = pump_until(&rt, &mut vm, "globalThis.got", 100);
    assert_eq!(got, "slept");
    assert_eq!(vm.pending_ops(), 0);
}

#[test]
fn op_ws_open_malformed_url_resolves_error_json() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test rt builds");
    let _guard = rt.enter();
    let (mut vm, _state) = bare_vm();
    vm.eval_string(
        "globalThis.got = ''; \
         op_ws_open('::bad', '[]').then(function (v) { globalThis.got = v; });",
    )
    .expect("call site arms");
    pump(&mut vm, 4);
    // URL::parse fails before any network: the open must fail as the
    // resolved {"error":...} JSON, not a rejected promise.
    let got = vm.eval_string("globalThis.got").unwrap();
    assert!(got.contains("\"error\":"), "resolved {got}");
    assert!(got.contains("invalid URL"), "resolved {got}");
}

#[test]
fn op_ws_next_message_on_dead_socket_resolves_close() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test rt builds");
    let _guard = rt.enter();
    let (mut vm, _state) = bare_vm();
    vm.eval_string(
        "globalThis.got = ''; \
         op_ws_next_message(9999).then(function (v) { globalThis.got = v; });",
    )
    .expect("call site arms");
    pump(&mut vm, 4);
    // Registry miss → Taken::Dead → close(1006) JSON, promise resolves.
    let got = vm.eval_string("globalThis.got").unwrap();
    assert!(got.contains("\"kind\":\"close\""), "resolved {got}");
    assert!(got.contains("\"code\":1006"), "resolved {got}");
}
