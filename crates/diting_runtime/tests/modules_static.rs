//! 认领战役批 10b 钉测（静态半边）：ESM 图编译、预解析加载、去重、
//! 环、import.meta、错误面——全部走自有 `ModuleMap`/`RecursiveLoad`，
//! 测试 loader 是本文件自己的，零 deno_core。
//!
//! 驱动法：`load_side_es_module_from_code` 是 poll_fn 造的 async 面，
//! 测试用 std-only 的 `block_on`（noop waker + 自旋）推；`mod_evaluate`
//! 的 future 同法——每自旋一圈先 `vm.poll()`（排水 microtask，TLA 的
//! then2 handler 才有机会跑）。这正是宿主在 10d 要写的循环形状。

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use diting_runtime::modules::{ModuleLoadResponse, ModuleLoader, ModuleLoaderError, ModuleSource};
use diting_runtime::vm::{LoopError, Vm};
use url::Url;

/// 内存 loader：name → 源码；`fetches` 数每个 URL 被真正 load 的次数
/// （in-flight 去重的证据就在这个计数上）。
struct TestLoader {
    modules: HashMap<String, String>,
    fetches: Cell<usize>,
}

impl TestLoader {
    fn new(entries: &[(&str, &str)]) -> Rc<Self> {
        Rc::new(Self {
            modules: entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            fetches: Cell::new(0),
        })
    }
}

impl ModuleLoader for TestLoader {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<Url, ModuleLoaderError> {
        // 根（referrer "."）：specifier 本身就是绝对 URL。referrer 不是
        // 合法 URL（经典脚本名）时退回按 specifier 直解析 —— 浏览器对
        // 无 base 的 import() 也是这个退化面。
        let resolved = Url::parse(specifier)
            .or_else(|_| Url::parse(referrer).and_then(|base| base.join(specifier)));
        resolved.map_err(|_| ModuleLoaderError::generic(format!("invalid specifier {specifier:?}")))
    }

    fn load(&self, url: &Url, _maybe_referrer: Option<&str>) -> ModuleLoadResponse {
        self.fetches.set(self.fetches.get() + 1);
        let code = self
            .modules
            .get(url.as_str())
            .cloned()
            .ok_or_else(|| ModuleLoaderError::generic(format!("module not found {url}")));
        ModuleLoadResponse::Sync(code.map(|code| ModuleSource { code }))
    }
}

/// std-only block_on：noop waker + 自旋。load 面的 poll_fn 不会真挂起
/// （loader 是 Sync 的），第一圈就 Ready；TLA 面靠 `pump` 推 microtask。
fn block_on<F: Future>(fut: F) -> F::Output {
    let noop_waker = Waker::noop();
    let mut cx = Context::from_waker(noop_waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

/// 把 `mod_evaluate` 的 future 跑到完：每圈先泵一轮 loop（settle 投递 +
/// microtask 排水），TLA 的续体才有地方跑。宿主循环的最小化身。
fn drive_evaluate(vm: &mut Vm, id: usize) -> Result<(), diting_runtime::ModuleEvalError> {
    let noop_waker = Waker::noop();
    let mut cx = Context::from_waker(noop_waker);
    let mut fut = Box::pin(vm.mod_evaluate(id));
    loop {
        if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
            return r;
        }
        vm.poll().unwrap();
    }
}

fn vm_with(entries: &[(&str, &str)]) -> Vm {
    let mut vm = Vm::new();
    vm.install_modules(TestLoader::new(entries));
    vm
}

fn load_root(vm: &mut Vm, name: &str, code: &str) -> Result<usize, LoopError> {
    block_on(vm.load_side_es_module_from_code(name, code.to_string())).map_err(|e| match e {
        diting_runtime::ModuleEvalError::Threw(m) => LoopError::TickThrew(m),
        diting_runtime::ModuleEvalError::Terminated => LoopError::Terminated,
    })
}

#[test]
fn static_import_graph_loads_and_evaluates() {
    let mut vm = vm_with(&[
        ("https://t.test/dep2.js", "export const v = 42;"),
        ("https://t.test/dep1.js", "export { v } from './dep2.js';"),
    ]);
    let code = "import { v } from './dep1.js'; globalThis.answer = v;";
    let id = load_root(&mut vm, "https://t.test/main.js", code).unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.answer").unwrap(), "42");
}

#[test]
fn import_edges_pre_resolve_through_the_loader() {
    // 相对 specifier 在编译期就解析成绝对 URL：main 里的 ./dep.js 变成
    // https://t.test/dep.js —— 模块名空间就是注册表的键。
    let mut vm = vm_with(&[("https://t.test/dep.js", "export const x = 1;")]);
    let id = load_root(
        &mut vm,
        "https://t.test/main.js",
        "import { x } from './dep.js'; globalThis.g = x;",
    )
    .unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.g").unwrap(), "1");
}

#[test]
fn diamond_graph_fetches_shared_once() {
    // 菱形：main → a + b，a、b 各自 → shared。shared 只被 fetch 一次
    // （去重法：已注册的名字直接复用，第二张边不再起 load future）。
    let loader = TestLoader::new(&[
        ("https://t.test/shared.js", "export const s = 7;"),
        (
            "https://t.test/a.js",
            "import { s } from './shared.js'; export const a = s + 1;",
        ),
        (
            "https://t.test/b.js",
            "import { s } from './shared.js'; export const b = s + 2;",
        ),
    ]);
    let fetches = {
        // TestLoader 是 Rc — 拿一个同门计数引用做断言。
        let probe = loader.clone();
        move || probe.fetches.get()
    };
    let mut vm = Vm::new();
    vm.install_modules(loader);
    let id = load_root(
        &mut vm,
        "https://t.test/main.js",
        "import { a } from './a.js'; import { b } from './b.js'; globalThis.sum = a + b;",
    )
    .unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.sum").unwrap(), "17");
    // 根是内联源码不 fetch：a(1) + b(1) + shared(1) = 3 —— shared 只算一次。
    assert_eq!(fetches(), 3, "diamond must not double-fetch shared");
}

#[test]
fn circular_imports_settle_with_live_bindings() {
    // 环：a → b → a。加载不挂死、不炸；ESM 活绑定让 b 拿到 a 的后期值。
    let mut vm = vm_with(&[(
        "https://t.test/b.js",
        "import { aVal } from './a.js'; export const bVal = aVal() + 1;",
    )]);
    // 根就叫 a.js —— b 的 ./a.js 解析回根，环才闭得上。环的经典解法
    // 是提升的函数导出：b 在 a 身体跑到一半时调 aVal()，const 导出还在
    // TDZ，函数导出已经可用（活绑定）。
    let code = "import { bVal } from './b.js'; \
                export function aVal() { return 41; } \
                globalThis.r = bVal;";
    let id = load_root(&mut vm, "https://t.test/a.js", code).unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.r").unwrap(), "42");
}

#[test]
fn import_meta_carries_url_and_main() {
    let mut vm = vm_with(&[]);
    let id = load_root(
        &mut vm,
        "https://t.test/main.js",
        "globalThis.metaUrl = import.meta.url; globalThis.metaMain = import.meta.main;",
    )
    .unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(
        vm.eval_string("globalThis.metaUrl").unwrap(),
        "https://t.test/main.js"
    );
    assert_eq!(vm.eval_string("globalThis.metaMain").unwrap(), "true");
}

#[test]
fn missing_dependency_surfaces_as_threw() {
    // 图里缺一个模块：loader 错误走 Threw（消息面），不是 panic。
    let mut vm = vm_with(&[]);
    let err = load_root(
        &mut vm,
        "https://t.test/main.js",
        "import { x } from './missing.js'; globalThis.x = x;",
    )
    .unwrap_err();
    match err {
        LoopError::TickThrew(m) => {
            assert!(m.contains("module not found"), "message was: {m}");
        }
        other => panic!("expected TickThrew, got {other:?}"),
    }
}

#[test]
fn syntax_error_in_dependency_carries_the_exception_message() {
    let mut vm = vm_with(&[("https://t.test/bad.js", "export const = ;")]);
    let err = load_root(&mut vm, "https://t.test/main.js", "import './bad.js';").unwrap_err();
    match err {
        LoopError::TickThrew(m) => {
            assert!(m.to_lowercase().contains("syntax"), "message was: {m}");
        }
        other => panic!("expected TickThrew, got {other:?}"),
    }
}

#[test]
fn root_dedup_second_load_of_same_graph_is_a_noop() {
    // 同名根第二次 load：已注册 → 直接复用 id，不再 fetch。加载面的
    // evaluate-once 法。
    let loader = TestLoader::new(&[("https://t.test/dep.js", "export const x = 1;")]);
    let probe = loader.clone();
    let mut vm = Vm::new();
    vm.install_modules(loader);
    let code = "import { x } from './dep.js'; globalThis.n = (globalThis.n ?? 0) + x;";
    let id1 = load_root(&mut vm, "https://t.test/main.js", code).unwrap();
    let after_first = probe.fetches.get();
    let id2 = load_root(&mut vm, "https://t.test/main.js", code).unwrap();
    assert_eq!(id1, id2, "same name re-registers as the same id");
    assert_eq!(probe.fetches.get(), after_first, "no refetch on dedup");
    drive_evaluate(&mut vm, id2).unwrap();
    assert_eq!(vm.eval_string("globalThis.n").unwrap(), "1");
}

#[test]
fn evaluate_is_idempotent() {
    // 已 Evaluated 的模块再 evaluate：立即 Ok，副作用不重放。
    let mut vm = vm_with(&[]);
    let id = load_root(
        &mut vm,
        "https://t.test/main.js",
        "globalThis.count = (globalThis.count ?? 0) + 1;",
    )
    .unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.count").unwrap(), "1");
}

#[test]
fn top_level_await_module_completes_under_the_host_pump() {
    // TLA：await 一个微任务级的 promise。future 只有在宿主泵过
    // microtask（poll 里的 checkpoint）之后才 Ready —— drive_evaluate 的
    // 循环形状就是为此。
    let mut vm = vm_with(&[]);
    let id = load_root(
        &mut vm,
        "https://t.test/main.js",
        "const v = await Promise.resolve('tla-ok'); globalThis.tla = v;",
    )
    .unwrap();
    drive_evaluate(&mut vm, id).unwrap();
    assert_eq!(vm.eval_string("globalThis.tla").unwrap(), "tla-ok");
}
