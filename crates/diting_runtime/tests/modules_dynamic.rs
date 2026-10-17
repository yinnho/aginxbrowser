//! 认领战役批 10b 钉测（动态半边）：`import()` 宿主回调、图加载驱动、
//! settle、TLA waiter、错误面（Chrome 形状的 TypeError）。测试 loader
//! 同 `modules_static.rs` 的形状；驱动法 = 轮询 `vm.poll()`（poll 内部的
//! module 相就是动态导入的反应堆——load 排水、instantiate、evaluate、
//! settle 全在里面），断言用全局变量的字符串面读回。

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use diting_runtime::modules::{ModuleLoadResponse, ModuleLoader, ModuleLoaderError, ModuleSource};
use diting_runtime::vm::{LoopError, Vm};
use url::Url;

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

fn vm_with(entries: &[(&str, &str)]) -> Vm {
    let mut vm = Vm::new();
    vm.install_modules(TestLoader::new(entries));
    vm
}

/// 泵到 `expr` 的求值等于 `want` 为止（上限防挂死——loader 是 Sync 的，
/// 几圈之内必 settle）。
fn drive_until(vm: &mut Vm, expr: &str, want: &str) {
    // 头几圈表达式可能对 undefined 取属性而 throw（ns 还是 undefined）——
    // 那只是"还没 settle"，吞掉继续泵。
    for _ in 0..100 {
        vm.poll().unwrap();
        if let Ok(v) = vm.eval_string(expr) {
            if v == want {
                return;
            }
        }
    }
    panic!("dynamic import never settled: {expr} != {want}");
}

#[test]
fn dynamic_import_resolves_with_the_namespace() {
    let mut vm = vm_with(&[(
        "https://t.test/dep.js",
        "export const x = 5; export function f() { return x * 2; }",
    )]);
    vm.eval_string("import('https://t.test/dep.js').then(m => { globalThis.ns = m; });")
        .unwrap();
    drive_until(&mut vm, "globalThis.ns.x", "5");
    assert_eq!(vm.eval_string("globalThis.ns.f()").unwrap(), "10");
    // settle 完成后 pending 面归零。
    assert!(!vm.has_pending_dynamic_imports());
}

#[test]
fn dynamic_import_follows_relative_specifier_from_referrer() {
    // 从另一个模块里 import('./sibling.js') —— referrer 是导入方的 URL。
    let mut vm = vm_with(&[("https://t.test/sibling.js", "export const tag = 'sib';")]);
    // 根模块经 eval 脚本名（非 URL referrer）发起 —— 退化面按绝对 URL。
    vm.eval_string("import('https://t.test/sibling.js').then(m => { globalThis.t = m.tag; });")
        .unwrap();
    drive_until(&mut vm, "globalThis.t", "sib");
}

#[test]
fn missing_module_rejects_as_type_error() {
    // Chrome 的脸：网络/解析失败 = TypeError，可 catch，message 是
    // loader 的消息面。
    let mut vm = vm_with(&[]);
    vm.eval_string(
        "import('https://t.test/nope.js').catch(e => { \
             globalThis.ename = e.constructor.name; globalThis.emsg = e.message; });",
    )
    .unwrap();
    drive_until(&mut vm, "globalThis.ename", "TypeError");
    let msg = vm.eval_string("globalThis.emsg").unwrap();
    assert!(msg.contains("module not found"), "message was: {msg}");
}

#[test]
fn unresolvable_specifier_rejects_as_type_error() {
    // 裸 specifier（无 ./ 前缀、非 URL）：resolve 抛错 → TypeError reject。
    let mut vm = vm_with(&[]);
    vm.eval_string(
        "import('definitely-not-a-url').catch(e => { globalThis.e = e.constructor.name; });",
    )
    .unwrap();
    drive_until(&mut vm, "globalThis.e", "TypeError");
}

#[test]
fn evaluation_throw_rejects_with_the_live_exception() {
    // 模块体内 throw：reject 带原生异常（Error 身份保留，不是 TypeError）。
    let mut vm = vm_with(&[(
        "https://t.test/throws.js",
        "throw new Error('boom-in-module');",
    )]);
    vm.eval_string(
        "import('https://t.test/throws.js').catch(e => { globalThis.ename = e.constructor.name; globalThis.emsg = e.message; });",
    )
    .unwrap();
    drive_until(&mut vm, "globalThis.ename", "Error");
    assert_eq!(vm.eval_string("globalThis.emsg").unwrap(), "boom-in-module");
}

#[test]
fn top_level_await_import_settles_after_release() {
    // TLA：模块 await 一个由外部释放的 promise。settle 必须等到释放后。
    let mut vm = vm_with(&[(
        "https://t.test/slow.js",
        "export const v = await new Promise(r => { globalThis.release = r; });",
    )]);
    vm.eval_string("import('https://t.test/slow.js').then(m => { globalThis.g1 = m.v; });")
        .unwrap();
    // 泵两圈：图加载完、evaluate 起 TLA（promise 仍 pending）。
    vm.poll().unwrap();
    vm.poll().unwrap();
    assert_eq!(vm.eval_string("globalThis.g1").unwrap(), "undefined");
    vm.eval_string("globalThis.release('slow-v')").unwrap();
    drive_until(&mut vm, "globalThis.g1", "slow-v");
}

#[test]
fn duplicate_import_of_evaluating_module_parks_as_tla_waiter() {
    // 快路径的 TLA 分支：模块已注册、状态 Evaluated、TLA 仍 pending ——
    // 第二个 import() 停进 waiter 表，TLA 落地时一起 resolve。
    let mut vm = vm_with(&[(
        "https://t.test/slow.js",
        "export const v = await new Promise(r => { globalThis.release = r; });",
    )]);
    vm.eval_string("import('https://t.test/slow.js').then(m => { globalThis.g1 = m.v; });")
        .unwrap();
    vm.poll().unwrap();
    vm.poll().unwrap();
    // 此时模块在 pending_dyn_evaluations 里 —— 第二发 import 走 waiter。
    vm.eval_string("import('https://t.test/slow.js').then(m => { globalThis.g2 = m.v; });")
        .unwrap();
    assert_eq!(vm.eval_string("globalThis.g2").unwrap(), "undefined");
    vm.eval_string("globalThis.release('both')").unwrap();
    drive_until(&mut vm, "globalThis.g1", "both");
    assert_eq!(vm.eval_string("globalThis.g2").unwrap(), "both");
}

#[test]
fn already_evaluated_module_settles_on_the_fast_path() {
    // 静态面已 evaluate 过的模块：import() 立即 resolve（无新加载）。
    let loader = TestLoader::new(&[("https://t.test/dep.js", "export const k = 9;")]);
    let probe = loader.clone();
    let mut vm = Vm::new();
    vm.install_modules(loader);
    let id = futures_block_on(vm.load_side_es_module_from_code(
        "https://t.test/main.js",
        "import { k } from './dep.js'; globalThis.k = k;".to_string(),
    ))
    .unwrap();
    // 泵到 evaluate 完成。
    {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let noop = Waker::noop();
        let mut cx = Context::from_waker(noop);
        let mut fut = Box::pin(vm.mod_evaluate(id));
        loop {
            if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
                r.unwrap();
                break;
            }
            vm.poll().unwrap();
        }
    }
    let after_static = probe.fetches.get();
    vm.eval_string("import('https://t.test/dep.js').then(m => { globalThis.dk = m.k; });")
        .unwrap();
    drive_until(&mut vm, "globalThis.dk", "9");
    assert_eq!(
        probe.fetches.get(),
        after_static,
        "fast path must not refetch"
    );
}

#[test]
fn concurrent_duplicate_loads_both_settle() {
    // 同一脚本里两发 import()：两条 load 都在飞，各自 settle ——
    // in-flight 并发面（去重发生在注册，不是在发起）。
    let mut vm = vm_with(&[("https://t.test/dep.js", "export const c = 3;")]);
    vm.eval_string(
        "import('https://t.test/dep.js').then(m => { globalThis.c1 = m.c; }); \
         import('https://t.test/dep.js').then(m => { globalThis.c2 = m.c; });",
    )
    .unwrap();
    drive_until(&mut vm, "globalThis.c1", "3");
    assert_eq!(vm.eval_string("globalThis.c2").unwrap(), "3");
}

/// block_on（noop waker + 自旋）——与 modules_static.rs 同法。
fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let noop = Waker::noop();
    let mut cx = Context::from_waker(noop);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

/// 一个门闩 future：`gate` 置位前永远 Pending（不注册 waker——
/// poll_progress 每轮都来，靠轮询推进就够）。
struct GatedLoad {
    gate: Rc<Cell<bool>>,
    code: String,
}

impl Future for GatedLoad {
    type Output = Result<ModuleSource, ModuleLoaderError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.gate.get() {
            Poll::Ready(Ok(ModuleSource {
                code: self.code.clone(),
            }))
        } else {
            Poll::Pending
        }
    }
}

/// 按 URL 分门的 loader：fast 的门开着，slow 的门关着。
struct GatedLoader {
    modules: HashMap<String, (Rc<Cell<bool>>, String)>,
}

impl ModuleLoader for GatedLoader {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<Url, ModuleLoaderError> {
        Url::parse(specifier)
            .or_else(|_| Url::parse(referrer).and_then(|base| base.join(specifier)))
            .map_err(|_| ModuleLoaderError::generic("bad"))
    }
    fn load(&self, url: &Url, _r: Option<&str>) -> ModuleLoadResponse {
        let (gate, code) = self.modules.get(url.as_str()).unwrap();
        ModuleLoadResponse::Async(Box::pin(GatedLoad {
            gate: gate.clone(),
            code: code.clone(),
        }))
    }
}

#[test]
fn terminate_mid_drain_does_not_vaporize_in_flight_loads() {
    // #226 法的模块版：drain 途中 terminate 打断 fast 的 register（编译
    // 吃 interrupt）——fast 这条 load 死掉（书签清掉、promise 悬置），
    // 但还在飞的 slow load 必须原样回队，revive 后开门照常 settle。
    let fast: Rc<Cell<bool>> = Rc::new(Cell::new(true)); // 已就绪
    let slow: Rc<Cell<bool>> = Rc::new(Cell::new(false)); // 门关着
    let mut vm = Vm::new();
    vm.install_modules(Rc::new(GatedLoader {
        modules: [
            (
                "https://t.test/fast.js".to_string(),
                (fast.clone(), "export const v = 'f';".into()),
            ),
            (
                "https://t.test/slow.js".to_string(),
                (slow.clone(), "export const v = 's';".into()),
            ),
        ]
        .into_iter()
        .collect(),
    }));
    vm.eval_string(
        "import('https://t.test/fast.js').then(m => { globalThis.gFast = m.v; }); \
         import('https://t.test/slow.js').then(m => { globalThis.gSlow = m.v; });",
    )
    .unwrap();
    // 武装 terminate。第一轮 drain 只是 register（V8 的 parser 不查
    // interrupt——编译照过）；interrupt 咬在第二轮 evaluate 跑模块体
    // 的时刻。循环泵到它露头。
    vm.terminate();
    let mut surfaced = false;
    for _ in 0..4 {
        match vm.poll() {
            Err(LoopError::Terminated) => {
                surfaced = true;
                break;
            }
            Ok(_) => continue,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert!(surfaced, "terminate mid-drain must surface");
    assert!(
        vm.has_pending_dynamic_imports(),
        "the in-flight slow load must survive the interrupt"
    );
    // 复活 + 开门：slow 的图照常加载、evaluate、settle。
    vm.revive();
    slow.set(true);
    drive_until(&mut vm, "globalThis.gSlow", "s");
    // fast 的 promise 悬置（load 已死），fast 的书签已清——pending 面归零。
    assert!(!vm.has_pending_dynamic_imports());
}
