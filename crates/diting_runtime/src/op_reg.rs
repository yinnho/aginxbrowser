//! # op_reg — our own op registration face
//!
//! 认领战役步骤⑤的注册面：`#[diting_op]` 生成的 wrapper 返回这里的
//! [`OpDecl`]（不再是 deno_core 的），宿主（Vm 或过渡适配器）拿一列
//! decl 建 [`OpCtx`] 并装进 context。
//!
//! 安装原语逐句学自 deno_core 0.411 `runtime/bindings.rs` 的
//! `op_ctx_plain_function`（MIT，Copyright 2018-2026 the Deno authors）：
//! 自由函数 op 不需要 FunctionTemplate——`v8::Function::builder_raw(
//! slow_fn).data(External(opctx_ptr))` 一步到位，External 里放的是
//! **堆上 Box 的 OpCtx 裸指针**，OpSet 持有这些 Box 直到 isolate 死亡
//! （deno 的同款寿命法：OpCtx lives until isolate disposal）。
//!
//! reentrancy_check 同样逐字拷贝自 deno_core `ops.rs`（debug 断言下的
//! 线程驻留守卫；release 构建零成本）。

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use crate::state::OpState;

/// The raw callback type the v8 crate expects — the same type our macro's
/// `v8_fn_ptr` wrapper is cast to.
pub type SlowFn = v8::FunctionCallback;

/// The usize-returning slow entry — deno's op2 shape (`SlowFnImplRef`);
/// our async-op batch (driver host swap) will call it directly when it
/// needs the return count.
pub type SlowFnImpl = fn(*const v8::FunctionCallbackInfo) -> usize;

/// Registration metadata for one op. Produced by `#[diting_op]` wrappers —
/// the fn-shaped face call sites already use.
pub struct OpDecl {
    pub name: &'static str,
    pub is_async: bool,
    pub is_reentrant: bool,
    /// JS-visible `fn.length` (JS args only, state args don't count).
    pub arg_count: u8,
    pub no_side_effects: bool,
    pub slow_fn: SlowFn,
    pub slow_function_impl: SlowFnImpl,
}

impl OpDecl {
    /// `const` because the macro binds it in an associated `const DECL` —
    /// the wrapper's reentrancy check needs a promoted `&'static OpDecl`.
    pub const fn new(
        name: &'static str,
        arg_count: u8,
        slow_fn: SlowFn,
        slow_function_impl: SlowFnImpl,
    ) -> Self {
        Self {
            name,
            is_async: false,
            is_reentrant: false,
            arg_count,
            no_side_effects: false,
            slow_fn,
            slow_function_impl,
        }
    }
}

/// Per-op context — what the wrapper's `args.data()` External points at.
/// Field order mirrors the macro's decode (`opctx.state`), not deno's.
pub struct OpCtx {
    pub decl: OpDecl,
    pub state: Rc<RefCell<OpState>>,
}

/// A built op set: contexts boxed on the heap (stable addresses — the
/// Externals point into these boxes) plus the shared state cell. Dropping
/// the OpSet while the isolate still runs installed ops would dangle the
/// Externals; hosts keep it for the isolate's lifetime (Vm does).
pub struct OpSet {
    // The Box is the point: v8 Externals pin the heap address of each OpCtx,
    // so Vec growth must move only the box pointers, never the pointees.
    #[allow(clippy::vec_box)]
    ctxs: Vec<Box<OpCtx>>,
    state: Rc<RefCell<OpState>>,
}

impl OpSet {
    pub fn new(decls: Vec<OpDecl>) -> Self {
        let state = Rc::new(RefCell::new(OpState::new()));
        Self {
            ctxs: decls
                .into_iter()
                .map(|decl| {
                    Box::new(OpCtx {
                        decl,
                        state: state.clone(),
                    })
                })
                .collect(),
            state,
        }
    }

    /// The shared state cell — hosts hand this to `op_state()`-style faces
    /// before first JS runs.
    pub fn state(&self) -> Rc<RefCell<OpState>> {
        self.state.clone()
    }

    pub fn len(&self) -> usize {
        self.ctxs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ctxs.is_empty()
    }

    /// Install every op onto `target` (the host's ops object or globalThis).
    /// Learned statement-for-statement from deno_core's
    /// `op_ctx_plain_function` — constructor Throw, side-effect face from
    /// the decl, `fn.length` = arg_count, name set on the function.
    ///
    /// `is_async` decls ship wrapped: the `__diting_wrap_async` stub
    /// (vm PRELUDE) allocates the ring-table promise, prepends its id,
    /// and returns the promise — deno_core's `setUpAsyncStub` law. The
    /// raw wrapper underneath receives `(promise_id, user args...)` and
    /// never touches a V8 scope for promise machinery.
    pub fn install_onto<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        target: v8::Local<'s, v8::Object>,
    ) {
        for ctx in &self.ctxs {
            // Box deref gives the stable heap address the External pins.
            let op_ctx_ptr = ctx.as_ref() as *const OpCtx as *mut c_void;
            let external = v8::External::new(scope, op_ctx_ptr);
            let f = v8::Function::builder_raw(ctx.decl.slow_fn)
                .data(external.into())
                .constructor_behavior(v8::ConstructorBehavior::Throw)
                .side_effect_type(if ctx.decl.no_side_effects {
                    v8::SideEffectType::HasNoSideEffect
                } else {
                    v8::SideEffectType::HasSideEffect
                })
                .length(ctx.decl.arg_count as i32)
                .build(scope)
                .expect("op function builds");
            let name = v8::String::new(scope, ctx.decl.name).expect("op name fits");
            f.set_name(name);
            if ctx.decl.is_async {
                let global = scope.get_current_context().global(scope);
                let wrap_key = v8::String::new(scope, "__diting_wrap_async")
                    .expect("wrap face name fits");
                let wrapped = global
                    .get(scope, wrap_key.into())
                    .and_then(|f| f.try_cast::<v8::Function>().ok())
                    .and_then(|wrap| {
                        wrap.call(
                            scope,
                            v8::undefined(scope).into(),
                            &[f.into()],
                        )
                    });
                match wrapped {
                    Some(wrapped) => {
                        target.set(scope, name.into(), wrapped);
                    }
                    None => {
                        // Not our shell (no stub face): install the raw fn
                        // anyway — calls then return undefined instead of a
                        // promise, loud in any test that exercises the op.
                        target.set(scope, name.into(), f.into());
                    }
                }
            } else {
                target.set(scope, name.into(), f.into());
            }
        }
    }
}

#[cfg(debug_assertions)]
thread_local! {
    static CURRENT_OP: std::cell::Cell<Option<&'static OpDecl>> = None.into();
}

#[cfg(debug_assertions)]
pub struct ReentrancyGuard {}

#[cfg(debug_assertions)]
impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        CURRENT_OP.with(|f| f.set(None));
    }
}

/// Creates an op re-entrancy check for the given [`OpDecl`].
/// Copied verbatim from deno_core 0.411 `ops.rs` (MIT, Deno authors).
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reentrancy_check(decl: &'static OpDecl) -> Option<ReentrancyGuard> {
    if decl.is_reentrant {
        return None;
    }

    let current = CURRENT_OP.with(|f| f.get());
    if let Some(current) = current {
        panic!(
            "op {} was not marked as reentrant, but re-entrantly invoked op {}",
            current.name, decl.name
        );
    }
    CURRENT_OP.with(|f| f.set(Some(decl)));
    Some(ReentrancyGuard {})
}

/// Release builds compile the guard out (deno's shape); the wrapper's
/// binding still resolves.
#[cfg(not(debug_assertions))]
#[doc(hidden)]
pub fn reentrancy_check(_decl: &'static OpDecl) -> Option<()> {
    None
}
