# diting_js(原 obscura_js)摸底报告 — 认领 Phase 0/Phase 1

> 2026-08-22 摸底盘点,2026-08-23 完成认领。这是四个模块里**最大、分歧最深**的一个:上游 runtime.rs 已从 2575 行涨到 16438 行(6.4 倍),233 个 commit。认领顺序 dom→net→**js**→browser。
>
> **Phase 1 完成(2026-08-23)**:A 组(已有等价)确认、B 组(~50 个同源 bug 修复)11 个主题全吸完(+47 特征测试,总 307 过)、模块改名 `obscura_js`→`diting_js`、`ObscuraJsRuntime`→`JsRuntime`(deno_core 同名类改全路径)。C 组大特性按条挂账见下。

## 一句话职责

V8 运行时 + JS↔DOM/Rust 桥:脚本执行、事件循环、watchdog 超时保护、fetch/XHR、cookie/URL/编码 op、CDP 求值入口、bootstrap JS 环境。它是"浏览器像真的"的主要战场——反检测、框架兼容(React/Vue)全在这层。

## 体量与文件

| 文件 | 行数(我们/上游) | 逐行相同率 | 职责 |
|---|---|---|---|
| `runtime.rs` | 2575 / 16438 | 75% | `JsRuntime`(原 ObscuraJsRuntime):V8 isolate 管理、evaluate、事件循环、watchdog、模块加载 |
| `ops.rs` | 1262 / 5605 | 76% | 16 个 op:op_dom(DOM 桥)、op_fetch_url、cookie/navigate/sleep/subtle_digest/url/encoding |
| `cdp_watchdog.rs` | 117 / 122 | 70% | CDP 求值 watchdog |
| `module_loader.rs` | 145 / 286 | 80% | ES 模块加载(import map 解析接线) |
| `import_map.rs` | 463 / 455 | ~95%(port) | HTML import maps(34373c3 吸收,见"已知坑"第 19 条) |
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
6. ✅ **fetch/XHR 组**(2026-08-22 完成,+3 测试 +1 reload 测试):
   - **4b90ec3(20 次重定向)**:redirect limit 10→20(spec 说 20);`redirects_followed > LIMIT` 递增后判定(20 过、21 挂)。
   - **3eb28da(FormData multipart)**:fetch body 序列化重写——FormData 分支处理 Blob/File 值(filename + Content-Type + bytes),`_bytesToBinaryString` Latin-1 转二进制;`FormData.append/set` 改 Blob 值保持对象(原 `String(v)` 全转字符串,File 变 "[object File]");补 `_bodyToUint8Array` 的 `_bytes` 短路。
   - **260c4c0(Blob/ArrayBuffer body)**:Blob body 分支(content-type 取自 blob.type)、ArrayBuffer/view 分支。
   - **b744b9b(跨域 credentials)**:新增 FetchCredentials(Omit/SameOrigin/Include)parse/allows + `request_origin`(url.origin().ascii_serialization() 规范化默认端口)+ `cors_response_allows`(Include 要求精确 ACAO + ACAC "true")。op_fetch_url 加第 8 参 credentials;cookie 发送与 set-cookie 存储都按 `credentials.allows(page_origin, current_url)` 门控(cookie 按 host 不按 origin/port,RFC 6265)。XHR withCredentials→include/same-origin。+2 op 单测 +1 三服务器 e2e。
   - **ab6fa0e(fetch 按 context 分 client,#453)**:op_fetch_url 用页面 context-scoped `http_client.request_client()`(diting_net/client.rs 新加 accessor,reqwest::Client clone 廉价共享连接池),不再共享进程级 FETCH_CLIENT_CACHE,避免顺序 V8 runtime 共用 async 连接池;无 owning HttpClient 的 bare runtime 回落 select_request_client。
   - **402de26(Blob-URL Worker race + location 同步)**:**Blob race 部分我们已有等价**——`__blobObjs` 同步存 Blob 对象(bootstrap.js:6833-6841),Worker resolveCode 先查 `__blobStore` 再落 `__blobObjs`(且我们 Blob.text() 同步),比上游只存 text 更全,不改动。location 部分全吸:href/assign/replace setter 改 `__virtualUrl = r` 同步预览(原 setter 前清 null,读回老 URL);**reload() 从 no-op 修成真导航**(挑战页 reload-after-token-cookie 场景);`__obscura_init` 清 `__virtualUrl = null` 让 document_url 重新驱动(含 redirect 目标)。+1 reload 测试。注:我们 op_navigate 本就同步更新 gs.url(ops.rs:1159),document_url 立即反映新 URL,所以同步预览对我们是 belt-and-suspenders,redirect 场景才真正靠它。
7. ✅ **structuredClone 组**(2026-08-22 完成,+3 测试):
   - **a921668(真 structuredClone,#389/#390)**:JSON.parse(JSON.stringify) 会把 ArrayBuffer/TypedArray 丢成 {},CF turnstile orchestrate 经 postMessage 回传字节全丢。替换为递归 `_structuredClone`:覆盖 ArrayBuffer/TypedArray/DataView(DataView 无 .slice(),按 view 区间切 buffer)/Map/Set/Date/RegExp/Error/普通对象,带 seen map 保循环引用与引用同一性、own symbol-keyed 属性;函数/symbol 抛 DataCloneError(放在 primitive 提前返回之前,否则被引用传递)。platform 对象经 `__obscura_clone_hooks[toStringTag]` 分发 hook。
   - **b2e4bb4(Error.cause 循环 + own `__proto__`,#419/#420)**:Error 分支先把克隆写进 seen 再递归 cause(e.cause===e 不再爆栈);普通对象克隆到 Object.prototype(不取源原型),own `__proto__` 数据属性用 defineProperty 定义(普通赋值会命中继承的 __proto__ setter 把克隆重挂),其余 key 走赋值快路径。
   - **挂账到 WebCrypto 主题(已解决,见第 8 条)**:a921668/8698afc 的 CryptoKey clone hook——WebCrypto 主题(ed75730)落地真 CryptoKey/keyMaterial 后已补齐。
8. ✅ **WebCrypto 组**(2026-08-22 完成,+8 测试):
   - **edde67d(拒绝未知算法,#314/#319)**:digest 对未知名(MD5 等)静默回落 SHA-256。bootstrap.js digest 先校验名字(SHA-1/256/384/512 + SHA-512/224 + SHA-512/256),不匹配抛 DOMException('NotSupportedError')。
   - **dc780d7(SHA-512/224、SHA-512/256,#314)**:op_subtle_digest 加 `sha2::Sha512_224`/`Sha512_256`(FIPS 180-4 截断变体,AWS WAF PoW worker 用),`_ => vec![]`。+1 测试:FIPS 180-4 向量 + MD5 抛 NotSupportedError。
   - **ed75730(真 SubtleCrypto 对称算法,#390)**:crypto.subtle 原来只有 digest,sign 返回固定 32 字节、verify 恒 true、encrypt/decrypt 抛、generateKey/importKey/deriveBits 给占位假数据——反 bot 探针/PKCE/客户端加密静默拿错结果。完整实现:**7 个新 Rust op**(op_subtle_hmac/aes_gcm/aes_cbc/aes_ctr/pbkdf2/hkdf + op_random_bytes CSPRNG)+ **8 个 RustCrypto 依赖**(hmac/aes/aes-gcm/cbc/ctr/pbkdf2/hkdf/getrandom,纯 Rust 无 CMake/OpenSSL)。bootstrap.js:crypto.subtle 重写为真实现 + 真 CryptoKey 类(keyMaterial WeakMap + makeKey/keyBytes)+ normalizeAlgo/normalizeHash/runOp 等。公钥算法(RSA/ECDSA/ECDH)与非对称 key 格式(pkcs8/spki)抛 NotSupportedError 不再给假数据。getRandomValues/randomUUID 从 Math.random 换 CSPRNG(顺带消除指纹暴露)。+4 测试:HAMC/AES-GCM/AES-CBC/PBKDF2 roundtrip(RFC 4231/RFC 6070 向量精确匹配)、CryptoKey 身份、CSPRNG。**顺带补齐 structuredClone 的 CryptoKey clone hook(a921668/8698afc)**。
   - **cfda91b(PBKDF2 DoS,#580)**:PBKDF2 迭代数与输出长度直接来自页面 JS、无上限,单线程 V8 会被 4294967295 次迭代钉死数小时、大 length 触发无界 `vec![0u8; length]` 分配。抽出 `pbkdf2_derive` helper,cap 迭代 ≤10M、输出 ≤1MiB(远超 OWASP ~600k 合法上限),越界抛 OperationError。+3 单测(迭代越界/长度越界/正常派生)。
9. ✅ **DOM 遍历(TreeWalker/NodeIterator)组**(2026-08-22 完成,+5 测试):上游实际是**七连**(c3ae054/c12915a/1a5c27a/a8c0a19/ab3ca26/49d4b91/845abb9),本文档旧"六连"误把 scroll 的 1c7402d 算进来、漏了 c3ae054+1a5c27a;改为按上游 HEAD 最终态整体吸收而非逐条打补丁。
   - **diting_dom/tree.rs**:新增 3 个遍历方法 + 1 个私有 helper(`climb_to_next_sibling`,带 `0..=nodes.len()` 上界防环)。`next_in_subtree`(first_child 优先,否则爬 next_sibling)/`next_after_subtree`(只爬 next_sibling,REJECT 剪子树用)/`prev_in_subtree`(逆文档序:prev_sibling 或 parent,否则 prev_sibling 的最深末后代)。
   - **ops.rs**:op_dom_inner 加 3 个命令(`next_in_subtree`/`next_after_subtree`/`prev_in_subtree`,NodeId u32,空返回 "-1")。
   - **bootstrap.js**:`createTreeWalker`/`createNodeIterator` 换上游最终版——**TreeWalker 的 nextNode/previousNode 永不返回 root**(指针语义);**NodeIterator 返回 root 第一**(指针在 root 之前),且 detach() 后 nextNode/previousNode 立即返 null(上游 a8c0a19:原实现 detach 后仍能用)。`_filter` 三值语义(1 ACCEPT/2 REJECT 剪子树/3 SKIP 下钻子节点)。补全 `NodeFilter` 常量集(SHOW_ELEMENT..SHOW_NOTATION + FILTER_ACCEPT/REJECT/SKIP),删除旧的残缺 NodeFilter 块。
   - **5 个测试**:next 文档序 + REJECT 剪枝、previousNode 逆序、parentNode 不出 root、SKIP 下钻、NodeIterator 返 root + detach。前 3 个断言第一版按"直觉"写错了(TreeWalker 不返 root),按规范修正。
10. ✅ **DOM 杂项组**(2026-08-22 完成,+16 测试):11 个 commit 全吸(注意 a2fd4d1 早在 documentElement.innerHTML 修复时已吸,见第 2 条)。
    - **c4f545e(DocumentFragment 拍平)**:insertBefore/replaceChild 补 fragment 拍平(appendChild 已有),按 childNodes 顺序插入子节点并清空 fragment。
    - **5177304(完整插入步骤)**:insertBefore/replaceChild 补 MutationObserver childList 报告(before/after/replaceWith 都走 insertBefore,原来全静默)。
    - **a16e8d4(checkbox/radio 默认 "on")**:value getter 对无 value 属性的 checkbox/radio 返 "on"(原返空串);显式 value 属性仍赢。
    - **25ce541(真 NodeList)**:`class NodeList extends Array` → 独立类型(Array.isArray false、`[object NodeList]`),带 item/forEach/entries/keys/values/迭代;childNodes 走 `_nodeList`。核对了内部所有 childNodes 用法(只 index/length/Array.from,兼容)。
    - **b460b37(adoptNode/toggleAttribute)**:Document.adoptNode(返回节点,无第二 document 即本属自己)、Element.toggleAttribute(带 force 语义),都在 prototype 上 polyfill + 标 native。
    - **491ecfb+90ed9af(cloneNode 结构化)**:元素 cloneNode 改用 Rust `dom.clone_node` op(tree.rs 新增,直接拷贝 NodeData 不序列化重解析,tr/td/option 不再被 div 上下文丢弃,保留元素类型/命名空间/template contents 独立重映射);非元素(文本/注释/fragment)走 `_shallowCloneNode` + 显式栈(防深嵌套爆 JS 栈)。**顺手抓到 ae438e1 的 JS 半侧缺口**:template_contents 桥我们只吸了 dom 半侧,`.content` getter 一直返回空假 fragment——补 `dom.template_contents`(按需分配 contents doc)+ op 命令 + getter 改从 Rust 取真实 contents 并按 nid 缓存保身份。
    - **a663a15(insertAdjacentHTML 大小写不敏感 + SyntaxError)**:position 已小写化(旧代码),补未知 position 抛 SyntaxError;上下文解析我们已用 a2fd4d1 的 createElement(contextTag)+innerHTML 方案(等价于上游 wrapMap,且更直接),未引入 `_parseHTMLFragment`/`_wrapMap`。
    - **ad7a7a9(dataset/style 的 in 与 Object.keys)**:dataset 半侧我们 ec05ed0 已完整(has/ownKeys/getOwnPropertyDescriptor 都有),跳过。style 半侧全吸:new CSSStyleDeclaration(dashed 名存储 + `_CSS_PROP_SET` 标准属性名单) + `_styleProxy` 加 has/ownKeys/getOwnPropertyDescriptor,`'gap' in el.style`/`Object.keys(el.style)` 与 camelCase↔dashed 同步、cssText 尾分号。核对了 `_obscuraFontBox` 与 getComputedStyle 两处直接读 `_props` 的消费者兼容。
    - **80803cb(style 双向同步)**:CSSStyleDeclaration 挂 owner element,`_pull`(读时惰性拉 style 属性,未变跳过重解析)+ `_push`(每次 mutation 序列化回 style 属性);parsed inline style/setAttribute('style')/el.style.x 三向一致;无 owner 的声明(getComputedStyle 回落、样式表规则)纯内存不变。Element 构造 `new CSSStyleDeclaration(this)`。
    - **41a8e1c(DOM 移动保 script 状态)**:核心全吸——ObscuraState 加 `already_started_scripts: RefCell<HashSet<NodeId>>`(原生 per-document 标志,活过 wrapper 换手/移动/cloneNode/fragment 解析)+ 两个 op(`op_script_mark_started`/`op_script_try_start` 原子认领)。bootstrap 抽 `__prepareInsertedScript`(先 try_start 门控,再走我们既有的 fetch/eval/module 执行逻辑)+ `__prepareInsertedSubtree`(树序收集 root+后代 script,isConnected 才准备),appendChild/insertBefore/replaceChild 改调它;set_inner_html 导入的子节点 `mark_script_subtree_started`(innerHTML script 惰性);clone_node op 后 `propagate_script_start_state`(克隆继承 started 状态)。**未吸收**:`__dynScriptQueue`/`__processDynScriptQueue` 串行队列(并发 import 重构,与本 bug 无关)、import-map 处理(C 组)、base[href] 解析、`set_fragment_html_executable`/Range.createContextualFragment 独立策略(我们没有该 API 通路)。
    - **16 测试**:3 fragment 拍平 + 1 mutation 报告 + 1 checkbox + 1 NodeList + 1 adoptNode/toggleAttribute + 3 cloneNode(浅克隆属性隔离/深克隆表结构+template/深嵌套不爆栈)+ 1 style in/keys + 1 dataset in/keys + 1 style 双向同步 + 1 insertAdjacentHTML + 3 script 状态(移动不重跑/克隆不重跑/innerHTML 惰性)。
11. ✅ **事件组**(2026-08-23 完成,+8 测试,改 1 个旧行为断言):9 个 commit 全查(旧清单把 2e3f5d8 误写成 2f3d5d8;3f820c4 只是 merge)。
    - **af1e15f(Event/CustomEvent 构造器 WebIDL)**:无参构造抛 TypeError("1 argument required")、type 经 String() 强转(`new Event(123).type === "123"`)、CustomEvent.detail 默认 null(非 undefined)。子类经 super 继承全部。
    - **776c915 + 0ff1ba0(PromiseRejectionEvent/StorageEvent)**:两个全局构造器补齐(core-js 靠前者探测环境,缺失会用破 polyfill 盖掉原生 Promise,Vue 渲染挂);PromiseRejectionEvent 的 promise 成员必填(缺失抛 TypeError,Chrome 同);StorageEvent 带 initStorageEvent legacy 路径。
    - **7e6f403(createEvent 拒绝未知)**:未知接口名抛 DOMException NotSupportedError(原静默回落通用 Event,把调用方 typo 藏成"init* 方法全缺");补 DOM Level 2 legacy 别名(Event/Events/HTMLEvents/SVGEvents)与 hashchangeevent/messageevent 映射;promiserejectionevent 故意不进 map(Chrome 也拒)。旧行为测试 `test_create_event_unknown_type_returns_event` 断言已翻转。
    - **2e3f5d8(iframe 事件)**:_IframeDocument 的 addEventListener/removeEventListener/dispatchEvent 从 no-op 换真实现(去重/移除/cancelable 返回值/on+type 属性);iframe load 从直接调 `el.onload()` 换成 `el.dispatchEvent(new Event('load'))`——我们的 Element.dispatchEvent 同时跑 on* 属性 + inline attr + addEventListener 监听器,旧直接调用路径全覆盖且不再漏监听器。
    - **scroll 三连(29e20ae/f6ca133/1c7402d)**:按上游 HEAD 终态整体吸收。Element scrollTop/scrollLeft 从恒 0 换真偏移跟踪(无布局故意不设上限——合成上限会把 scrollTop 钉死在 0,懒加载死锁);scrollTo/scrollBy/scroll 三方法支持 (x,y) 与 ScrollToOptions 两种形参;直接赋值变化时发 scroll 事件、scroll 操作用 _scrollSuppress 把每轴事件合并成每操作一个;window 级 scrollTo/scrollBy/scroll 从空 stub 换真实现,偏移存在 scrollingElement(=documentElement)上——window.scrollY 与 scrollingElement.scrollTop 是同一个值的两面;scrollX/scrollY/pageXOffset/pageYOffset 从硬编码 0 数据属性换成只读 accessor;窗口滚动事件同时发到 document 和 window(Document.dispatchEvent 不传播,只发一边会漏一半监听器);无变化提前返回。**连带升级**:session.rs 的 MCP scroll 命令走 `window.scrollBy`,现在真的会移偏移+发事件,无限滚动页翻页从"只拿到一屏"变成能触发加载。
    - **08c1f0d(React/Vue controlled input)**:已有等价且更优,不改动——on* 事件属性我们已有 accessor 版(bootstrap.js `_GLOBAL_EVENT_NAMES`,比上游 value:null 数据属性多了 JS 赋值存储 + inline content attribute 编译回落);两条 typing 路径(session.rs `input_by_index` 用 `_valueTracker.setValue('')` 重置 + prototype setter;firecrawl_compat.rs 直接 prototype setter)已等价实现"绕开 React value tracker 让 onChange 触发"。上游的 `__obscura_setFieldValue` 全局 helper 无新消费方,不引入。
12. ✅ **计时器组**(2026-08-23 完成,+2 测试):3 个 commit 全吸。
    - **452cc85(字符串 timer handler)**:我们的 setTimeout/setInterval 对字符串 handler 是**整个丢弃**(静默 no-op 还返回 timer id,比上游修复前的 new Function 还糟)——`setTimeout("loadMore()", 500)` 这种老式页面代码完全不跑。补 `_coerceTimerFn`:字符串包成 fire-time 间接 eval `() => { (0, eval)(src); }`,顶层 var/function 声明成为真全局(new Function 包裹会留在函数局部),语法错误在触发时抛而非调度时吞。
    - **cdab919+d93ff51(performance.now 单调有界,按终态合并)**:我们的 now() 直接返回 `Date.now()` 原值——timeOrigin 被 init 设成 epoch 后 `performance.now()` 返回 ~1.7e12 而非"距导航起点毫秒数"(真浏览器是小值,指纹/计时探针可辨)。换终态实现:module 级 `_perfLast` 单调下限(同毫秒允许相等、不合成增量,紧循环不会跑赢真实流逝时间),timeOrigin 动态查找(字面量里初始为 0,init 构造后才赋真值,闭包捕获会拿错)。**跨导航重启语义我们天然更强**:每次导航整个重建 runtime(page.rs init_js,防跨页面状态攻击的安全决策),_perfLast 是新闭包,无需上游式 reset。
13. ✅ **Location 组**(2026-08-23 完成,+2 测试):2 个实质 commit(旧清单的 7404366 是 7404362 merge 的笔误,内容即 fe26417)。
    - **fe26417(导航值 String 强转)**:我们的 `_resolveUrl` 对 URL 对象调 `.startsWith` 直接抛 TypeError——`location.href = new URL(...)`/`assign`/`replace` 传 URL 对象全炸。首行补 `url = String(url)`(URL 对象 String() 得 href,命中绝对 URL 分支)。document.location setter 原有的 String() 包裹保留(belt-and-suspenders)。
    - **1fc5a24(pushState/replaceState 缺省 url 保当前 URL)**:我们的 `resolveOrFallback` 同上游 bug——缺省 url 返回 undefined,applyVirtual 把 `__virtualUrl` 清空,`pushState({}, '', '/dashboard')` 后再 `replaceState({scroll:1})` 会把 location 弹回原 document URL。改为缺省返回 `__currentUrl()`(HTML 规范:缺省 url 保留当前文档 URL);初始 entry 的 url: undefined 语义不变。
14. ✅ **脚本加载组**(2026-08-23 完成,+4 测试):6 个 commit 全吸(按上游 HEAD 终态,分 Rust 半侧与 bootstrap 半侧两次落地)。
    - **d3a8b9a+be700f5(模块完成/失败传播)+ 4f6d256 半侧(重复求值)**,Rust 侧整体重写 `load_module`/`load_inline_module`:手动 fetch 入口保持我们的架构(上游 PreparedModule prepare/evaluate 拆分服务于我们没有的加载策略,不引入),但求值驱动抽出 `drive_module_eval`——deno_core 0.350 的 `mod_evaluate` 对同一模块重复调用会 panic("Module already evaluated"),用 `catch_unwind` 捕获后归一为 Ok;求值结果(`module_evaluations: HashMap<ModuleId, Result<(),String>>`)缓存,后续同模块加载直接命中,堆不再随重复 import 线性涨;事件循环驱动改为 pinned `select! biased`(event_loop 先驱完再 poll result,原来 result 先 ready 会把未跑完的微任务截断);加载图与求值都套 `timeout(budget_ms)`(调用方 page.rs 传 10s),非 2xx 模块响应现在报 "Module {} returned HTTP {}" 而非把错误页当代码求值。`loaded_module_specifiers` 跟踪不需要——我们的路径每次 from_code 全新 ModuleId,无上游"同 URL 复用已求值模块"的通路。
    - **0c4740a+f841205(data: URL 脚本)**,bootstrap 侧:`op_fetch_url` 的 HTTP client 不支持 data: scheme,动态 `<script src="data:...">` 在我们这里整条死路。新增 `_decodeDataScriptUrl`(上游终态逐字节对齐:手写 data: 解析、base64 双重校验+padding 归一、`_hexv` 字节循环 percent-decode、非 ASCII 经 TextEncoder/TextDecoder UTF-8 往返),接进 `__prepareInsertedScript` classic 分支——data: 在 JS 侧解码,不进 HTTP client;MIME 无关(Chromium 对动态 data 脚本不校验 MIME)。hide list 是动态的(`_` 前缀自动隐),无需上游的静态名单登记。
    - **f61493f(拒绝失败响应)**:classic 分支拿到 `parsed.status` 后,非 2xx 直接 throw('HTTP ' + status)——404/500 的诊断 HTML(常含可执行 JS 片段)永不成为脚本源。**顺手修了我们自己的缺口**:error 路径原来只调 `script.onerror` 属性、从不 `dispatchEvent(new Event('error'))`,纯 addEventListener 消费者收不到失败。
    - **a6bb741(动态脚本 settle 延长)——不是 N/A,真吸了**:上游修的是"动态脚本 fetch 慢于 settle 500ms deadline 就被掐,onload 永远不 fire"。我们 settle loop 与上游修复前逐字相同,且动态脚本 fetch 走 ops 侧 `FETCH_CLIENT_CACHE`(独立 reqwest),page 级 `http_client.active_requests()` **根本看不见它在飞**。修法对齐上游语义:ObscuraState 加 `dynamic_script_fetches: Cell<u32>` + 两个 fast op(`op_dyn_script_fetch_begin/end`)从 bootstrap 网络分支 begin/finally-end 括号;runtime 暴露 `has_pending_dynamic_scripts()`;page.rs settle loop 保 500ms 快路径,deadline 到后仅当动态脚本在飞才续泵,硬上限 `OBSCURA_DYNAMIC_SCRIPT_SETTLE_MS`(默认 3s)+ watchdog 同步延长。普通页面与无关 XHR 不受影响(与上游注释的 fast-path 承诺一致)。
    - **4 测试**:data: 全变体执行(空 MIME/percent-escape/非 ASCII UTF-8 往返/fragment/带填充与无填充 base64)+ 坏 base64 双变体只 error 不 eval + 404 body 永不执行(本地 server)+ 慢脚本 300ms 期间计数器可见为 pending、落盘后归零且脚本生效。
15. ✅ **DOMParser XML 组**(2026-08-23 完成,+2 测试):4 个 commit 按上游 HEAD 终态整体吸收(53295fa regex 栈校验 → 6927f11 XML mime 才检查 → 869f700 self-closing 计完整元素 → 20c4628 化简;终态实际是**两层并存**:`_checkXmlWellFormed` regex 栈给具体错误消息 + `_xmlWellFormed` 手写状态机(引号感知 `>` 扫描/未终结 tag/严格单根 rootsClosed===1)兜底,后者会覆盖前者的输出成通用消息)。
    - **两层校验全吸** + parseFromString 三段(isParserError 分支/HTML parse/状态机兜底覆盖)+ documentElement 在 parsererror 时返回 firstElementChild + querySelector 的 root 自匹配 fallback。regex 版剥注释/CDATA/PI/DOCTYPE;状态机版同样跳过且更严(纯文本输入=零根也是 malformed——regex 版漏这种)。
    - **对上游的一处偏差(有意)**:上游用 `root.innerHTML = '<parsererror>...'` 构造,我们的 html5ever fragment 解析把未知元素路由进 `<head>`,firstElementChild(=documentElement)会是 HEAD 而非 parsererror。改为 `createElement('parsererror') + appendChild` 直接构造,可观察行为对齐 Chrome(documentElement 就是 parsererror,querySelector 命中)。
    - **2 测试**:6 变体(mismatch×2/extra root/unclosed→E:PARSERERROR;well-formed/self-closing root→OK)+ 3 变体(纯文本零根兜底/HTML mime 完全不走校验/CDATA+注释+PI+DOCTYPE 噪声不误报)。
16. ✅ **表单组**(2026-08-23 完成,+2 测试):5 个 commit 中 3 个全吸;6788996+c2b79b6(DOM.setFileInputFiles CDP 文件上传)挂账——我们没有 CDP DOM domain,MCP 会话也无文件上传命令入口,bootstrap 半侧(el.files/FileList/C:\fakepath)无消费方,未来加上传命令时照上游模式补。
    - **7e2cabf(submit/requestSubmit 语义分裂)**:我们的 `submit()` 发 cancelable submit 事件——上游修的正是这个:页面的 submit listener preventDefault 后再调 `form.submit()`(invisible-reCAPTCHA data-callback 模式)会被自己的 listener 拦死,死循环。拆三方法:`submit()` 直通 `_navigateSubmit`(不发事件、listener 不可否决);`requestSubmit(submitter?)` 发 cancelable 事件,未取消才导航;`_navigateSubmit` 是原 body。新增 `_isSubmitButton` helper(button 排除 reset/button;input 只认 submit/image)。
    - **ccfa5fb(requestSubmit submitter 校验)**:非 submit 按钮 → TypeError("not a submit button");submit 按钮但不属于本 form → DOMException NotFoundError("not owned by this form element");两个检查都在事件发出前跑。click() 的 submit-button 分支同上游改走 `form.requestSubmit(this)`(内部 click 永远不会递给它一个会被拒的 submitter;click 是"用户发起"要发事件)。
    - **5308e04(select 三项)**:① `type` getter 补 IDL 固定类型——select-one/select-multiple/textarea(jQuery 的 select valHook 按 type 分标量/数组,空串让所有单选读成数组);② `selectedIndex` getter 单选 select 无选中隐式选第一项返 0,multiple 空选返 -1(原一律 -1);③ select 的 value 赋值不再发 change 事件(Puppeteer page.select 模式:赋值后自己在页面里补发 input/change;在 change handler 里赋值会无限自触发)。顺带吸 `select.add(option|optgroup, before?)`(数字 before 当索引,校验参数 TypeError)。
    - **既有测试兼容**:`test_submit_button_click_handler_can_prevent_default_and_navigate` 走 click→requestSubmit 路径,事件照发、preventDefault 照拦,无需改。
17. ✅ **杂项组**(2026-08-23 完成,+4 测试):5 个 commit 全吸。**B 组(~50 个同源 bug 修复)至此全部过完。**
    - **a5a8de7+891d850(new Image() 真元素化)**:我们的 Image shim 正是上游修复前的 bare class(无 .style,addEventListener 是空函数,src 赋值只碰 onload 属性)。换终态:`Image` 改工厂函数,内部 `document.createElement('img')`(style/属性反射/事件派发全白拿),prototype 指到 HTMLImageElement.prototype(instanceof 成立);src setter 委托原型 accessor 后模拟解码成功——complete 翻转 + setTimeout(0) dispatchEvent('load')(onload 属性与 listener 都能收到,懒加载器不再挂死);Booking.com 式预定义 non-configurable own src 时跳过模拟不炸构造器。
    - **fc9f524(NetworkInformation)**:我们的 `navigator.connection` 是纯数据对象,addEventListener 压根不存在(比上游修复前还裸)。补 NetworkInformation 类(downlink/downlinkMax/effectiveType/rtt/saveData/type + onchange/ontypechange 属性 + 三事件方法,dispatchEvent 也跑 on* handler)+ `_markNative`,connection 挂单例。
    - **edb1785(document.referrer,全链路)**:我们完全没有 referrer。四层落地——ObscuraState.referrer 字段 + `document_referrer` op 命令(注意 bootstrap `_dom` 的 `_domStrA1` 名单要登记,否则无 nid 调用被"Illegal invocation"门控吞成空串,踩过)+ runtime `set_referrer` + page.rs `navigation_referrer`(strict-origin-when-cross-origin:同源发完整 URL 去 fragment/凭据,跨源只发 origin/,https→http 与非 HTTP(S) 空)+ 接线(automation 导航入口清空,JS 触发的导航链每跳按上一 URL 盖章,init_js 重建 runtime 时带入)。document.title setter 我们 5b4dc7a 已有且更强(带 Rust 侧 set_document_title op 同步,导航响应能用);DOMParser doc 补 title 空白折叠 + title setter + 空 referrer。
    - **5c3d560(脚本错误隔离)**:test-only——上游加回归锁定 #147 已修行为(一个 inline script 抛错不断掉后续 script)。行为我们已有(pois-guard),照上游加锁定测试(execute_script s1/s2 throw/s3,__ran1 与 __ran3 都 true,错误消息透传)。
    - **4 测试**:Image 真元素(style 赋值/instanceof/双路 load/劫持 createElement 后不炸且 width 保留)+ NetworkInformation(fc9f524 同款断言 + on* 属性双跑)+ referrer 语义(默认空/set_referrer 透出)+ 脚本错误隔离(5c3d560 同款)。
18. ✅ **getBoundingClientRect 真布局接线**(2026-08-24 完成,+2 测试,自研非吸收):V8 路径的 rect 从合成散点(12 列伪网格,nid 哈希定 (x,y),为 Playwright hit-testing 设计,issue #45)换成 diting_css + diting_layout 全管线的真几何。三层落地——`DomTree::epoch()`(nodes.len<<8 | free_list.len&0xFF,树形变即失效)+ JsState `layout_cache`(epoch 键控 memo,op 侧新命令 `layout_rect(nid)` 返回 `[x,y,w,h]`;computed styles 不缓存——style/class 属性写不 bump epoch,每次重算级联防陈旧几何;`#[cfg(feature="screenshot")]` 门控,diting_layout 的 taffy/swash 重依赖不进无渲染构建,无 feature 时命令返 null)+ bootstrap.js getBoundingClientRect 先查真 rect(命中构造 DOMRect 形状含 top/right/bottom/left/toJSON),null/异常回落合成网格保 Playwright 兼容。viewport 定宽 1920(与 bootstrap innerWidth persona 一致)。**连带行为升级**:elementFromPoint 从"合成散点碰运气"变真命中测试((10,10) 现在正确落在全宽 h1 上,原测试期望 BODY 是合成网格下的错误行为,已更新)。session_state 的 indexed rect 与 /eval 的 getBoundingClientRect 同源同真。冒烟:两 button 页 #b x=20 是自身 margin-left:20px 的真实效果,width:50% div 出 960=1920/2。
19. ✅ **import maps(34373c3)**(2026-08-25 完成,+10 测试):C 组大特性里唯一纯 JS 吸收项。新文件 `diting_js/import_map.rs`(455 行,port 上游 SGavrl 实现):HTML import map 算法全量——exact/prefix 双匹配、scopes 最长前缀优先、多 map merge(已观测 (referrer,specifier) 解析冻结,新规则不得改写;unrelated 新规则保留)、prefix 回溯检测、integrity 成员形状校验(完整性执行归 fetch 层,形状错则整张 map 作废)。五处接线:(a) `DitingModuleLoader::resolve` 走 `ImportMap::resolve`,deno_core 合成 "." referrer(load_side_es_module 根)直通 base_url 不进 map——`<script type=module src>` 根 URL 是资源 URL,map 不得重映射;(b) JsState 加 `import_map: Rc<RefCell<ImportMap>>`,runtime 构造时 clone 同一 Rc 给 loader+state,parser/dynamic/loader 三方共见一张 map;(c) op `op_add_import_map(source, base_url)→error string`;(d) bootstrap.js `__prepareInsertedScript` 认 `type=importmap`:动态插入的 map 在插入点注册(用 live baseURI),src 版报"External import maps are not supported",解析失败 console.error + microtask 派发 error 事件;(e) page.rs `execute_scripts` ScriptKind::{Classic,Module,ImportMap} 三分类 + per-script base_url(树走一遍跟踪 `<base href>` 遇见序,later base 不 rebase earlier map)+ importmap 相位先于模块图启动统一注册。**未吸收**(同 commit 的调度重构,与本特性正交):PreparedModule fetch/evaluate 两段式、encounter-order 统一调度、execute_classic_script 真 URL referrer(deno_core execute_script 只收 &'static str name)。我们保持 classic(regular→deferred→async)后接模块相位的既有顺序——对标准页面(map 在 head、module 在后)语义等价。外部 import map(`<script type=importmap src>`)与 multiple-maps-per-document 全量规范仍不跟(上游也不支持 src 版)。
20. ✅ **postMessage targetOrigin 门禁(上游 issue #704,2026-08-25 完成,+2 测试)**:上游 mnaza 报的 HIGH——targetOrigin 参数被全链路丢弃,`frame.contentWindow.postMessage(token, 'https://trusted.example')` 照样送达错误 origin 的 frame,跨源数据泄漏。探针证实我们同洞(`MSG_TO_TRUSTED_ONLY@undefined` 送达)。修法:两条投递路径(_IframeWindow.postMessage 父→iframe 方向、self-targeted `window.postMessage`)统一门禁——undefined/null 视为 '*'、非字符串或空串丢弃、'*' 直达、'/' 要求与调用文档同源、显式 origin 必须匹配目标 window 自身 origin,不匹配**静默丢弃**(浏览器语义,不抛)。连带修复:lazily 创建 iframe browsing context 时 URL 从 src 属性推导(浏览器在 context 创建时即定),不再落 about:blank——否则 targetOrigin 检查永远看到空 origin。测试:单 async eval 驱动三态(mismatch 落空/match 送达/'*' 通配)+ 同源 '/' 与 self-targeted 门控。
21. ✅ **fetch scheme 门禁拒 file://(上游 issue #708,2026-08-25 完成,+1 测试)**:`validate_fetch_url` 原来放行 file:// 且短路 SSRF 检查(与导航的 deny-by-default 姿态不一致;transport 反正抓不到 file:,但门禁给探测脚本漏了"file 允许"信号)。改为只允许 http/https,file:// 前置拒绝("Forbidden URL scheme 'file' - only http and https are allowed"),SSRF 短路塌缩成仅 private-network flag。回归测试钉三态:file 拒/ftp 拒/https 过。JS 边界仍报不透明 `net::ERR_FAILED`(bootstrap 把 op 错误包成网络失败信封,与 Chrome 对页面脚本的报法一致),区分细节在服务端日志。
22. ✅ **相对 URL 按 <base href> 解析(上游 issue #658,2026-08-25 完成,+1 测试)**:anchor/area href、form action、iframe src、location 强转、fetch/XHR 相对输入全部错用裸 document URL,无视 `<base href>`。修法:ops 加 `document_base_url` 命令(HTML document-base-url 算法——document URL 折叠第一个 base[href],无 base 或解析失败回落原 URL);bootstrap `_docBase()` helper 统一接所有解析路径。**身份面不动**:`document.URL`/`document.documentURI` 与 origin 检查仍读裸 document URL——base 改变"相对 URL 的含义",不改变"文档来自哪"。测试:锚点 href + form action + 真·本地服务器记录 fetch 实际请求路径(`/assets/data.json`)+ document.URL 不变。测试设计坑:data: 页上折叠 base 是 no-op(URL parser 拒绝对不透明路径 join),复现必须用 http(s) 页。

## 上游这两个月(233 commits)分类

**A. 已有/自研等价(不用吸):** document.title setter(我们 5b4dc7a ≈ edb1785 的一半,referrer 语义没有)、Plugin/MimeType globals(a8358cc ≈ 我们 53d2a0f)、template contents 桥(ae438e1,dom 侧已吸)、setAttribute 命名空间(6314ecb/549,已吸)、stealth 指纹一致性(4309935,我们 d478bdb 自研)。

**B. 同源 bug 修复,认领时逐条评估吸收(~50 个,按主题):**
- ~~**fetch/XHR**:4b90ec3 20 次重定向、3eb28da FormData multipart、260c4c0 Blob/ArrayBuffer body、b744b9b 跨域 credentials、ab6fa0e fetch 按 context 分 client、402de26 Blob-URL Worker race、bd39512 intercept 重写 SSRF 复检(安全)~~ ✅ 已吸(2026-08-22,bd39512 见安全组,其余见"已知坑"第 6 条)
- ~~**structuredClone**:a921668 真实现、b2e4bb4 循环引用/cause、8698afc CryptoKey seen map~~ ✅ 核心已吸(2026-08-22,见"已知坑"第 7 条);CryptoKey clone hook 挂账到 WebCrypto 主题
- ~~**WebCrypto**:ed75730 SubtleCrypto 对称算法、dc780d7 SHA-512 变体、edde67d 拒绝未知算法、cfda91b PBKDF2 上限(DoS)~~ ✅ 全吸(2026-08-22,见"已知坑"第 8 条)
- ~~**DOM 遍历**:TreeWalker/NodeIterator(c3ae054/c12915a/1a5c27a/a8c0a19/ab3ca26/49d4b91/845abb9 七连)~~ ✅ 已吸(2026-08-22,见"已知坑"第 9 条)
- ~~**DOM 杂项**:c4f545e DocumentFragment 拍平、491ecfb+90ed9af cloneNode 结构化、a663a15+a2fd4d1 insertAdjacentHTML 上下文、25ce541 真 NodeList、a16e8d4 checkbox 默认 "on"、b460b37 adoptNode/toggleAttribute、ad7a7a9 dataset/style 的 in 与 Object.keys、80803cb style 双向同步、41a8e1c DOM 移动保 script 状态、5177304 完整插入步骤~~ ✅ 已吸(2026-08-22,见"已知坑"第 10 条)
- ~~**事件**:0ff1ba0+af1e15f 构造器 WebIDL 语义、776c915 PromiseRejectionEvent/StorageEvent、7e6f403 createEvent 拒绝未知、2e3f5d8 iframe 事件、scroll 四连(1c7402d/29e20ae/f6ca133/3f820c4)、08c1f0d React/Vue controlled input~~ ✅ 已吸(2026-08-23,见"已知坑"第 11 条)
- ~~**计时器**:452cc85 字符串 handler 当脚本跑、cdab919 performance.now 单调、d93ff51 时钟有界~~ ✅ 已吸(2026-08-23,见"已知坑"第 12 条)
- ~~**Location**:fe26417/7404366 导航值强转、1fc5a24 pushState 缺省保 URL~~ ✅ 已吸(2026-08-23,见"已知坑"第 13 条)
- ~~**脚本加载**:d3a8b9a 模块完成才返回(Vite mount)、be700f5 模块失败传播、4f6d256 重复模块/堆耗尽、0c4740a+f841205 data: URL 脚本、f61493f 拒绝失败响应、a6bb741 动态脚本 settle~~ ✅ 已吸(2026-08-23,见"已知坑"第 14 条)
- ~~**DOMParser XML**:53295fa+6927f11+869f700+20c4628 parsererror~~ ✅ 已吸(2026-08-23,见"已知坑"第 15 条)
- ~~**stealth/反射(与 Radar 对抗直接相关)**:4c33f6d/c7e7c70/846ed7d/a0e1ba5/ec05ed0/9dfc67a~~ ✅ 已吸(2026-08-22)
- ~~**表单**:7e2cabf submit 语义、ccfa5fb requestSubmit 校验、5308e04 select parity、6788996+c2b79b6 文件输入~~ ✅ 3/5 已吸(2026-08-23,见"已知坑"第 16 条);6788996+c2b79b6 挂账(无 CDP DOM domain/上传命令入口)
- ~~**杂项**:a5a8de7 真 new Image()、891d850 img src configurable、5c3d560 脚本错误隔离、fc9f524 NetworkInformation 监听、edb1785 referrer 语义~~ ✅ 已吸(2026-08-23,见"已知坑"第 17 条)

**C. 大特性/架构,不跟或挂账:**
- **08-03/08-04 渲染浪潮(~60 commits)**:Shadow DOM、live CSSOM、Web Animations、Canvas2D paint、layout/geometry、PDF、响应式图片——服务上游自研渲染器,我们渲染走 blitz,不跟。
- **iframe realm 架构**(frame.rs 新文件:6a4683d/964bace/a954149/49e5605/3db9c60 postMessage)——真实站点兼容需要,但牵动架构,挂账到 browser 认领时评估。
- **V8 并发架构**(76fc3b9 每连接独立线程、9065f38 lock 分片、9d2f9f2 watchdog slot)——我们是全局锁单 isolate 模型,改架构风险大,先读懂再定。
- ~~import maps(34373c3)~~ ✅ 已吸(2026-08-25,见"已知坑"第 18 条)、custom elements 构造器升级(9bacacc,挂账)、模块图抓取(b1aec0c/1d2dc4e,配 net 的 subresource,挂账)。
- ~~⚠️ 319c603 revert cancellation-safe~~ 已解决:revert 针对的是手写 poll 泵,与我们无关;线程封闭我们已有,缺的构造序列化+主线程预热已补(见"核心结构"节)。

## 认领建议(Phase 1 开工顺序)

1. ~~读懂 319c603 + 并发三连~~ ✅ 2026-08-22 完成:结论=我们的模型(timeout(run_event_loop) + session 独占线程 + per-call watchdog)与上游 revert 后的终态一致;补了 isolate 构造锁 + 主线程预热;删三个死模块。
2. **修既有挂账**:fetch base64 测试、d0d8617(head)、0ca7ac0(document.write)。
3. **安全优先**:bd39512(intercept SSRF)、cfda91b(PBKDF2 DoS)、4f6d256(堆耗尽)。
4. ~~**stealth/反射组**(4c33f6d/c7e7c70/846ed7d/a0e1ba5/ec05ed0/9dfc67a)~~ ✅ 2026-08-22 完成(见"已知坑"第 5 条)。
5. **fetch/DOM/事件组**按主题批量过,每组补特征测试。~~fetch/XHR 组~~ ✅、~~structuredClone 组~~ ✅、~~WebCrypto 组~~ ✅、~~DOM 遍历组~~ ✅、~~DOM 杂项组~~ ✅、~~事件组~~ ✅(2026-08-22/23,见"已知坑"第 6/7/8/9/10/11 条);~~计时器组~~ ✅(2026-08-23,见"已知坑"第 12 条);~~Location 组~~ ✅(2026-08-23,见"已知坑"第 13 条);~~脚本加载组~~ ✅(2026-08-23,见"已知坑"第 14 条);~~DOMParser XML 组~~ ✅(2026-08-23,见"已知坑"第 15 条);~~表单组~~ ✅(2026-08-23,见"已知坑"第 16 条);~~杂项组~~ ✅(2026-08-23,见"已知坑"第 17 条)。**B 组全部完成。**
6. ~~**改名 `diting_js`**,类型 `ObscuraJsRuntime`→`JsRuntime`~~ ✅ 2026-08-23 完成:目录 git mv、全 crate 引用替换、`ObscuraJsRuntime`→`JsRuntime`(与 deno_core 同名类撞名,deno_core 侧改 `deno_core::JsRuntime` 全路径)。Extension 名已是 `diting_dom`(dom 认领时改过)。
   **符号层扫尾(2026-08-24)**:残留的 `Obscura*` 类型全部改净——`ObscuraState`→`JsState`、`ObscuraModuleLoader`→`DitingModuleLoader`(js 层),`ObscuraSelector(Parser)`→`DitingSelector(Parser)`(dom selector.rs),`ObscuraElemName`→`DitingElemName`(tree_sink.rs)。**JS 协议面同步改**:宿主↔脚本全局 `__obscura_*`→`__diting_*`(runtime.rs 41 处 + bootstrap.js 65 处,含内部 helper `__obscuraPlatformFromUA` 等),两侧一次改完无中间态;stealth 洗刷探测词同步扩成 obscura+diting 都查(runtime.rs:4020)。保留:`RenderTier::Obscura` API 枚举值(客户端契约)、上游出处注释。453 测试全绿,已部署 86quan 验证 engine:"diting"。
7. **C 组挂账**:渲染浪潮不跟;iframe/并发架构读完写结论到本文档。
