# obscura_js 摸底报告 — 认领 Phase 0

> 2026-08-22 摸底盘点。这是四个模块里**最大、分歧最深**的一个:上游 runtime.rs 已从 2575 行涨到 16438 行(6.4 倍),233 个 commit。认领难度最高,放第三位(dom→net→**js**→browser)。

## 一句话职责

V8 运行时 + JS↔DOM/Rust 桥:脚本执行、事件循环、watchdog 超时保护、fetch/XHR、cookie/URL/编码 op、CDP 求值入口、bootstrap JS 环境。它是"浏览器像真的"的主要战场——反检测、框架兼容(React/Vue)全在这层。

## 体量与文件

| 文件 | 行数(我们/上游) | 逐行相同率 | 职责 |
|---|---|---|---|
| `runtime.rs` | 2575 / 16438 | 75% | `ObscuraJsRuntime`:V8 isolate 管理、evaluate、事件循环、watchdog、模块加载 |
| `ops.rs` | 1262 / 5605 | 76% | 16 个 op:op_dom(DOM 桥)、op_fetch_url、cookie/navigate/sleep/subtle_digest/url/encoding |
| `cdp_watchdog.rs` | 117 / 122 | 70% | CDP 求值 watchdog |
| `module_loader.rs` | 115 / 286 | 80% | ES 模块加载 |
| `markdown.rs` | 71 / 71 | 100% | HTML→Markdown JS 片段 |
| `v8_flags.rs` | 37 / 37 | 100% | V8 启动 flags |
| `v8_lock.rs` | 27 / —(上游无此文件) | — | 全局 V8 锁(自研) |
| `mod.rs` | 12 / 23(lib.rs) | — | re-export |

共 4216 行,8 文件,72 个测试(71 在 runtime.rs)。上游新增文件:`frame.rs`(37KB,iframe realm)、`import_map.rs`(15KB)、`write_stream.rs`(7.8KB,document.write 流)——全是这两个月的新架构。

## 核心结构

```
ObscuraJsRuntime { isolate + OpState + watchdog + object store }
OpState { dom: DomTree, http_client: diting_net::HttpClient, cookie_jar,
          url/encoding/title, blocked_urls, pending_navigation,
          pending_binding_calls, intercept_tx/enabled }
ops.rs = deno_core 风格 #[op2] 函数,JS bootstrap 经 op_dom 操作 diting_dom
WatchdogToken = IsolateHandle + 超时线程,terminate_execution 兜底
```

关键设计:**单线程 V8;session 独占 OS 线程**(session.rs:每个 session 一个专用线程,channel 派发命令——自研实现,恰是上游 76fc3b9 "thread-per-connection" 的等价物);一次性操作(fetch/eval/click)走 `spawn_blocking`,一个闭包一个临时 runtime,`block_on` 天然线程封闭。watchdog 用 `terminate_execution` 防页面 JS 死循环(我们 3fa7586/460dc1a/fc6edc2,**per-call 独立线程**,无全局槽位)。

**架构分岔口已解决(2026-08-22,Phase 1 第一步):** 上游 319c603 revert 的是"手写 poll_fn 逐 tick 泵事件循环"——其前提错误(deno_core 的 `run_event_loop` 本来就是 `poll_fn(poll_event_loop)`,poll 间 drop 安全,isolate 每次 poll 后退出),且引入了两个回归(tick_ms 语义反转爬死 promise 链、吞事件循环错误饿死模块)。**revert 后的上游做法 = `timeout(run_event_loop())`,正是我们一直在用的模式,无冲突、无需改。** 真正修 #430(并发页面 isolate 串线 abort)的是线程封闭,我们已有。已落地:
- 删 `v8_lock.rs`/`cdp_watchdog.rs`/`v8_flags.rs`(全是无调用方的死代码;cdp_watchdog 的全局单槽正确性依赖一个没人持有的锁,删掉即消除 9d2f9f2 类隐患)
- 吸收上游并发修复中我们缺的部分:`ISOLATE_CONSTRUCT_LOCK` 序列化 isolate 构造(V8 JSDispatchTable 初始化并发不安全,我们并发建 session 有真暴露)+ main 线程 V8 预热(首个 isolate 在非主线程创建有 segfault 史)
- 去掉 mod.rs 的 `#![allow(dead_code/unused_imports)]`,12 条既有死代码警告现形(InterceptedRequest/object store/markdown const 等上游 API 面未被服务层消费,留作认领路标,不急着删)

## 服务层依赖点

| 消费方 | 用了什么 |
|---|---|
| `obscura_browser/page.rs` | 最重:ObscuraJsRuntime 全 API(evaluate/CDP/模块加载/事件循环) |
| `src/main.rs` | runtime 构造、v8_flags 初始化 |
| `ops.rs` → `diting_dom` | JS DOM API 的地基(dom 认领时的最大消费方就是这里) |
| `ops.rs` → `diting_net` | op_fetch_url 走 HttpClient;intercept 经 InterceptedRequest |

对外接口面集中在 `ObscuraJsRuntime`,但行为面极宽(每个 op 都是 JS 可见语义)。

## 已知坑/挂账(认领时处理)

1. ✅ **`test_fetch_url_input_decodes_binary_body_base64`**(2026-08-22 修):根因不是 fetch 路径,是我们自己的 stealth 加固——bootstrap 删了 `Deno` 全局(Radar 对抗),而测试猴子补丁 `Deno.core.ops.op_fetch_url` 必然失败;上游没删 Deno 所以同样的测试在那边是绿的。改写为真实本地 HTTP 服务器端到端测(URL 输入解析 + base64 二进制体),顺带覆盖了 op 的真实链路。
2. ✅ **documentElement.innerHTML 丢 head**(2026-08-22 修):吸收 d0d8617(find_body_or_root→fragment_root 不下钻合成 body)+ 其前置 a2fd4d1(片段按插入元素上下文解析:tree_sink 加 parse_fragment_with_context,set_inner_html 带上下文)。
3. ✅ **document.write 单输入流**(2026-08-22 修):吸收 0ca7ac0——新文件 write_stream.rs(195 行,每文档一个持久 html5ever Parser,Tracer 收集未完节点,script/template 完整才移交),ops 加 document_write/document_write_reset 两个命令,bootstrap write() 带插入点跟踪(__currentScriptNid 锚点)。**顺手抓到两个我们独有的 bug**:bootstrap 旧 insertAdjacentHTML 边遍历活 childNodes 边移动节点(隔一个丢一个),旧 document.write 同款;改为 firstChild 弹出循环。insertAdjacentHTML 另修复按插入上下文解析(tr/td 不再被 div 上下文丢弃)。未吸收:0ca7ac0 的 window 命名访问测试(我们没有 window[name] 注册基建,记为挂账)。
4. ✅ **安全组**(2026-08-22 完成):
   - **bd39512(intercept SSRF)**:吸收时发现我们比上游更糟——Continue 的 url/method/headers/body 重写**整个被丢弃**(`_new_url` 下划线变量,只 log 不应用)。现已应用重写 + 重写 URL 过 validate_fetch_url 复检(与重定向同门),补测试。
   - **cfda91b(PBKDF2 DoS)**:N/A——我们 deriveBits/deriveKey 是 stub(bootstrap.js:6934),没有迭代循环,无暴露面。
   - **4f6d256(堆耗尽)**:吸收 heap-limit 守卫(near-heap-limit 回调 terminate 当前脚本 + 64MB 喘息空间让 V8 展开,recover_heap_limit 挂在 evaluate/evaluate_with_timeout/run_event_loop 三个入口)——原行为是整进程 abort,一条 session 的分配循环会杀掉所有 session。**未吸收的另一半**:重复模块求值去重(loaded_specifiers/module_evaluations 两张表)深嵌上游 PreparedModule 图加载生命周期,我们的模块求值路径还是老的,挂账到脚本加载组(d3a8b9a/be700f5)一起评估。
5. ✅ **stealth/反射组**(2026-08-22 完成,六连全吸,243 通过/0 失败,+7 测试):
   - **4c33f6d**:反射过滤从我们原有的 gopn 单点补丁(pattern-based,本就比上游 list-based 覆盖面大)扩展到 `Reflect.ownKeys`/`Object.keys`/`Object.getOwnPropertyDescriptors` 四个 API;hide_list 改 `getOwnPropertyNames` 构建;新增文件尾 `_markBuiltinsNative` 快照时全局扫描(大写全局构造器 + 其 prototype 方法/访问器全标 native)。未吸收 `_nativeStr`/`_markNativeAs`——我们的 toString 模板用 `this.name`,getter 天然输出 `function get x() { [native code] }`,等价。
   - **c7e7c70**:我们没有上游 `_preHideInternals` 机制,改为文件尾 `_interfaceGlobalsNonEnumerable` 动态扫描(大写全局 → enumerable:false,保 configurable 不破坏 `var Node` 页面),比上游静态名单覆盖更全(含 Attr/ValidityState 等尾部定义)。
   - **846ed7d**:toString override 改 method-syntax 提取(name="toString"、length=0、不可构造、无 own prototype)。
   - **a0e1ba5**:`globalThis.CSSStyleDeclaration` 补值(我们的类是词法绑定,window 上原本 undefined)。
   - **ec05ed0**:新增 DOMStringMap 类(Illegal constructor + toStringTag),dataset 代理目标换成真实例,补全 has/delete/ownKeys/getOwnPropertyDescriptor 陷阱;**顺手修了我们独有的两个缺陷**:旧 dataset 每次访问新建 Proxy(`el.dataset === el.dataset` 为 false)且无 delete/keys 反射。
   - **9dfc67a**:`self.constructor === Window` 身份(Ember 等框架环境门)。

## 上游这两个月(233 commits)分类

**A. 已有/自研等价(不用吸):** document.title setter(我们 5b4dc7a ≈ edb1785 的一半,referrer 语义没有)、Plugin/MimeType globals(a8358cc ≈ 我们 53d2a0f)、template contents 桥(ae438e1,dom 侧已吸)、setAttribute 命名空间(6314ecb/549,已吸)、stealth 指纹一致性(4309935,我们 d478bdb 自研)。

**B. 同源 bug 修复,认领时逐条评估吸收(~50 个,按主题):**
- **fetch/XHR**:4b90ec3 20 次重定向、3eb28da FormData multipart、260c4c0 Blob/ArrayBuffer body、b744b9b 跨域 credentials、ab6fa0e fetch 按 context 分 client、402de26 Blob-URL Worker race、**bd39512 intercept 重写 SSRF 复检(安全)**
- **structuredClone**:a921668 真实现、b2e4bb4 循环引用/cause、8698afc CryptoKey seen map
- **WebCrypto**:ed75730 SubtleCrypto 对称算法、dc780d7 SHA-512 变体、edde67d 拒绝未知算法、cfda91b PBKDF2 上限(DoS)
- **DOM 遍历**:TreeWalker/NodeIterator 六连修(ab3ca26/a8c0a19/1c7402d/49d4b91/c12915a/845abb9)
- **DOM 杂项**:c4f545e DocumentFragment 拍平、491ecfb+90ed9af cloneNode 结构化、a663a15+a2fd4d1 insertAdjacentHTML 上下文、25ce541 真 NodeList、a16e8d4 checkbox 默认 "on"、b460b37 adoptNode/toggleAttribute、ad7a7a9 dataset/style 的 in 与 Object.keys、80803cb style 双向同步、41a8e1c DOM 移动保 script 状态、5177304 完整插入步骤
- **事件**:0ff1ba0+af1e15f 构造器 WebIDL 语义、776c915 PromiseRejectionEvent/StorageEvent、7e6f403 createEvent 拒绝未知、2f3d5d8 iframe 事件、scroll 四连(1c7402d/29e20ae/f6ca133/3f820c4)、08c1f0d React/Vue controlled input
- **计时器**:452cc85 字符串 handler 当脚本跑、cdab919 performance.now 单调、d93ff51 时钟有界
- **Location**:fe26417/7404366 导航值强转、1fc5a24 pushState 缺省保 URL
- **脚本加载**:d3a8b9a 模块完成才返回(Vite mount)、be700f5 模块失败传播、4f6d256 重复模块/堆耗尽、0c4740a+f841205 data: URL 脚本、f61493f 拒绝失败响应、a6bb741 动态脚本 settle
- **DOMParser XML**:53295fa+6927f11+869f700+20c4628 parsererror
- ~~**stealth/反射(与 Radar 对抗直接相关)**:4c33f6d/c7e7c70/846ed7d/a0e1ba5/ec05ed0/9dfc67a~~ ✅ 已吸(2026-08-22)
- **表单**:7e2cabf submit 语义、ccfa5fb requestSubmit 校验、5308e04 select parity、6788996+c2b79b6 文件输入
- **杂项**:a5a8de7 真 new Image()、891d850 img src configurable、5c3d560 脚本错误隔离、fc9f524 NetworkInformation 监听、edb1785 referrer 语义

**C. 大特性/架构,不跟或挂账:**
- **08-03/08-04 渲染浪潮(~60 commits)**:Shadow DOM、live CSSOM、Web Animations、Canvas2D paint、layout/geometry、PDF、响应式图片——服务上游自研渲染器,我们渲染走 blitz,不跟。
- **iframe realm 架构**(frame.rs 新文件:6a4683d/964bace/a954149/49e5605/3db9c60 postMessage)——真实站点兼容需要,但牵动架构,挂账到 browser 认领时评估。
- **V8 并发架构**(76fc3b9 每连接独立线程、9065f38 lock 分片、9d2f9f2 watchdog slot)——我们是全局锁单 isolate 模型,改架构风险大,先读懂再定。
- import maps(34373c3)、custom elements 构造器升级(9bacacc)、模块图抓取(b1aec0c/1d2dc4e,配 net 的 subresource)。
- ~~⚠️ 319c603 revert cancellation-safe~~ 已解决:revert 针对的是手写 poll 泵,与我们无关;线程封闭我们已有,缺的构造序列化+主线程预热已补(见"核心结构"节)。

## 认领建议(Phase 1 开工顺序)

1. ~~读懂 319c603 + 并发三连~~ ✅ 2026-08-22 完成:结论=我们的模型(timeout(run_event_loop) + session 独占线程 + per-call watchdog)与上游 revert 后的终态一致;补了 isolate 构造锁 + 主线程预热;删三个死模块。
2. **修既有挂账**:fetch base64 测试、d0d8617(head)、0ca7ac0(document.write)。
3. **安全优先**:bd39512(intercept SSRF)、cfda91b(PBKDF2 DoS)、4f6d256(堆耗尽)。
4. ~~**stealth/反射组**(4c33f6d/c7e7c70/846ed7d/a0e1ba5/ec05ed0/9dfc67a)~~ ✅ 2026-08-22 完成(见"已知坑"第 5 条)。
5. **fetch/DOM/事件组**按主题批量过,每组补特征测试。
6. **改名 `diting_js`**,类型 `ObscuraJsRuntime`→`JsRuntime`。
7. **C 组挂账**:渲染浪潮不跟;iframe/并发架构读完写结论到本文档。
