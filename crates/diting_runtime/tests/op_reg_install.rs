//! 认领战役步骤⑤钉测：自有 op 注册面端到端。
//!
//! `#[diting_op]` 生成的 wrapper（宏 + op_glue 全是我们自己的代码）+
//! `OpSet`（自有 OpCtx/OpDecl/state）+ 裸 `Vm` 安装——全程零 deno_core。
//! 刻意不测的两张面（宏的发射还引用 deno_core，宿主换批翻掉后这里补）：
//! state 参数（OpCtx decode 还是 deno 的）与 Result 返回（Err 臂走
//! deno 的 to_v8_error）。同步数值/字符串/解码失败面在这钉死。

use diting_ops::diting_op;
use diting_runtime::op_reg::OpDecl;
use diting_runtime::vm::Vm;

#[diting_op]
fn __t_add(a: u32, b: i32) -> i32 {
    a as i32 + b
}

#[diting_op]
fn __t_f(x: f64) -> f64 {
    x * 2.0
}

#[diting_op]
fn __t_bool(x: bool) -> bool {
    !x
}

#[diting_op]
fn __t_smi(#[smi] n: u32) -> f64 {
    n as f64 + 0.5
}

#[diting_op]
#[string]
fn __t_echo(#[string] s: &str) -> String {
    format!("echo:{s}")
}

#[diting_op]
#[string]
fn __t_own(#[string] s: String) -> String {
    s.to_uppercase()
}

#[diting_op]
#[buffer]
fn __t_rev(#[buffer] b: &[u8]) -> Vec<u8> {
    b.iter().rev().copied().collect()
}

#[diting_op]
fn __t_unit(#[string] s: &str) {
    let _ = s;
}

fn vm_with_ops() -> Vm {
    let mut vm = Vm::new();
    let decls: Vec<OpDecl> = vec![
        __t_add(),
        __t_f(),
        __t_bool(),
        __t_smi(),
        __t_echo(),
        __t_own(),
        __t_rev(),
        __t_unit(),
    ];
    vm.install_ops(decls);
    vm
}

#[test]
fn sync_ops_decode_and_encode_roundtrip() {
    let mut vm = vm_with_ops();
    assert_eq!(vm.eval_string("__t_add(2, 3)").unwrap(), "5");
    // u32 4294967295 wraps to -1 through the `as i32` cast inside the op —
    // the deliberate wrap-on-overflow semantics the glue doc notes.
    assert_eq!(vm.eval_string("__t_add(4294967295, -1)").unwrap(), "-2");
    assert_eq!(vm.eval_string("__t_f(2.25)").unwrap(), "4.5");
    assert_eq!(vm.eval_string("__t_bool(true)").unwrap(), "false");
    assert_eq!(vm.eval_string("__t_smi(7)").unwrap(), "7.5");
    assert_eq!(vm.eval_string("__t_echo('hi')").unwrap(), "echo:hi");
    assert_eq!(vm.eval_string("__t_own('abc')").unwrap(), "ABC");
    assert_eq!(
        vm.eval_string("__t_rev(new Uint8Array([1,2,3])).join(',')")
            .unwrap(),
        "3,2,1"
    );
    assert_eq!(vm.eval_string("__t_unit('x')").unwrap(), "undefined");
}

#[test]
fn fn_length_reflects_arg_count() {
    let mut vm = vm_with_ops();
    // state args don't exist here; every arg is JS-visible.
    assert_eq!(vm.eval_string("__t_add.length").unwrap(), "2");
    assert_eq!(vm.eval_string("__t_echo.length").unwrap(), "1");
}

#[test]
fn decode_failure_throws_type_error_with_message() {
    let mut vm = vm_with_ops();
    let err = vm.eval_string("__t_add('x', 1)").unwrap_err();
    let msg = match &err {
        diting_runtime::vm::LoopError::TickThrew(m) => m.clone(),
        other => panic!("expected TickThrew, got {other:?}"),
    };
    assert!(msg.contains("expected u32"), "message was: {msg}");
    // And the vm survives a thrown decode error — next call is fine.
    assert_eq!(vm.eval_string("__t_add(1, 1)").unwrap(), "2");
}

#[test]
fn constructor_call_throws() {
    let mut vm = vm_with_ops();
    // Installed with ConstructorBehavior::Throw (the learned primitive).
    let err = vm.eval_string("new __t_add(1, 1)").unwrap_err();
    assert!(matches!(err, diting_runtime::vm::LoopError::TickThrew(_)));
}

#[test]
fn ops_survive_poll_rounds() {
    // The OpSet lives in the Vm (Externals point into it); loop passes must
    // not disturb the installed functions.
    let mut vm = vm_with_ops();
    assert_eq!(vm.eval_string("__t_add(20, 22)").unwrap(), "42");
    vm.poll().unwrap();
    assert_eq!(vm.eval_string("__t_add(20, 23)").unwrap(), "43");
}
