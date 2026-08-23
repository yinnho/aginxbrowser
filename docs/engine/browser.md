# diting_browser 摸底报告 — 认领 Phase 0（Phase 1 已完成）

> 2026-08-23。引擎认领第四个（最后一个）模块。前三：`diting_dom`（43 测试）、
> `diting_net`（60 测试）、`diting_js`（307 测试，Phase 1 完成）。
> 本报告为 Phase 0 摸底：先读懂现有 1828 行，再量化与上游的 6400 行差距，
> 最后给出 Phase 1 分批认领建议。Phase 1（批次 0–3）已全部完成，模块
> 已改名 `diting_browser`，343 测试。

## 0. 一句话定位

diting_browser（认领前名 obscura_browser）是「没有渲染的浏览器」：**Page**（一个标签页 = DOM + JS realm +
导航历史 + 网络事件）+ **BrowserContext**（配置聚合：cookie/代理/UA/stealth）+
**lifecycle**（Load/DCL/NetworkIdle 等待语义）。真正的浏览器外壳——CDP 协议层、
截图、PDF、多 frame——上游放在别的 crate（obscura-cdp / obscura-render）或
page.rs 的渲染段落里，我们则放在自己的服务层（server.rs / session.rs /
screenshot.rs / mcp.rs）。

## 1. 规模对比

| | 我们 | 上游 (crates/obscura-browser) |
|---|---|---|
| page.rs | 1568 | 6772 |
| context.rs | 211 | 269 |
| lifecycle.rs | 42 | 42 |
| mod.rs | 7 | lib.rs 22 |
| pdf.rs | — | 1027（PDF 导出，依赖 obscura-render） |
| profiles.rs | 165（批次 1 吸收） | 103 |
| fork_virtual_url.rs | 44（批次 1 吸收） | 43 |
| **合计** | **1828** | **8256** |

2026-06-19 内联快照后，上游对该 crate 有 **68 个 commit**。差距不是「我们落后
6400 行没抄」，而是三条线：①渲染浪潮（~25 commit，走 obscura-render，与我们
Blitz 路线战略分歧）②frame realm 架构（8 commit）③可吸收的同源修复与功能
（其余）。

## 2. 文件清单与职责

### context.rs (211) — 配置聚合体
`BrowserContext` pub 字段：`cookie_jar / http_client / user_agent /
proxy_url / robots_cache+obey_robots / stealth / allow_file_access /
storage_dir / allow_private_network / tls_fingerprint`。全部字段为 pub 直接
读写——没有封装，Page 与服务层共享 `Arc<BrowserContext>`。
构造函数坍缩到 `_new_inner`：建 CookieJar → 从
`{storage_dir}/cookies.json` 恢复 → 建 HttpClient（with_full_options）→
UA 解析链（参数 → `AGINXBROWSER_UA` 环境变量 → 指纹池默认）→ UA
同步进 http_client。批次 3 删除了无消费者的 new/with_storage/
with_storage_full/with_proxy，留 with_storage_and_network（服务层唯一
入口）、with_full_options/with_options（测试矩阵）。
安全开关成对且注释明确威胁模型：`allow_file_access`（file:// 读）与
`allow_private_network`（SSRF 私网门）相互独立。
`save_cookies()` 落盘——**批次 3 记档：未接线**（服务层每请求新建
Browser 且不传 storage_dir，无拥有 jar 的关停时刻；加载半边已工作，
会话持久化立项时再接）。
**依赖点：diting_net 三件套（CookieJar/HttpClient/RobotsCache）——net 认领
完成后此文件已是薄壳。**

### lifecycle.rs (42) — 两个枚举
`LifecycleState`（Idle/Loading/DomContentLoaded/Loaded/NetworkIdle/Failed）+
`WaitUntil`（Load/DCL/NetworkIdle0/NetworkIdle2，`from_str` 吃 puppeteer/
playwright 双方拼写）。与上游逐行相同。

### page.rs (1568) — 核心，见 §4。

### mod.rs (7)
`pub use page::Page; pub use context::BrowserContext;`。批次 3 摘掉了
`#![allow(dead_code)]`——全部死项已按三档处置（见批次 3 记录）。

## 3. 服务层依赖点（认领时不能碰断的线）

服务层**全部**通过包装层访问内核，无一文件直接 `use crate::diting_browser::Page`：

```
src/browser.rs (107)  Browser = Arc<BrowserContext> 工厂；new_page() 发号 page-N
                       （进程级 AtomicU64）；cookies() 暴露 CookieStore
src/page.rs   (233)   Page 包装：goto/url/evaluate(+async,+timeout)/content/
                       query_selector/wait_for_selector/wait_for_cookie/
                       settle/settle_until_idle/pump_event_loop_slice/scroll_by
                       Element 句柄：text/attribute/click（scrollIntoView+click,
                       全部走 evaluate_with_timeout，INTERACTION_EVAL_TIMEOUT=10s）
```

实际调用方：
- **session.rs**（唯一重度用户）：goto/settle_until_idle/evaluate_with_timeout/
  evaluate_async/pump_event_loop_slice/process_pending_navigation——有状态
  session 的命令循环靠 pump_event_loop_slice 维持"活页"语义
- **render.rs / screenshot.rs / firecrawl_compat.rs / mcp.rs / server.rs**：经
  Browser→Page 包装层或更上层抽象，不直接持有 InnerPage

## 4. page.rs 解剖（1568 行）

### 4.1 顶层工具（1–151）
- `decode_data_uri/percent_decode/hex_val` — data: URI 内联解码（脚本与文档
  两条路共用）
- `cross_scheme_to_file` — SOP 门：JS 发起的导航不得从 http(s) 跨进 file:
- `subresource_allowed` — 子资源策略：http(s)/data: 放行；file: 仅当页面自身
  是 file:（防 `<script src=file:///etc/passwd>`）
- `escape_for_js_template_literal` — CSS 注入 JS 的模板字面量转义（U+2028/
  2029/控制字符全谱，防逃逸）
- `navigation_referrer` — strict-origin-when-cross-origin（上游 edb1785，
  wave 3 已吸收）

### 4.2 Page 结构（153–199，25 字段）
状态：`url/dom/js/lifecycle/title/referrer/encoding`；历史：`history+
history_index`（URL 串数组+游标，push 截断 forward）；网络：
`network_events+network_event_counter`（自增 request_id）；拦截：
`intercept_enabled/intercept_block_patterns/intercept_tx`；预载：
`preload_scripts`（CDP addScriptToEvaluateOnNewDocument 契约）；风暴退避：
`storm_backoff_ms/storm_hot_until`（watchdog 终止后指数退避 200ms–5s 停泵）；
`stealth_client`（feature 门）。
**frame_id == id（Chromium 惯例，playwright 主 frame 按 targetId 查找）。**

### 4.3 方法分组
1. **构造/init_js (202–344)**：stealth client 惰性建（TLS 指纹 → wreq
   Emulation，SOCKS5 拒绝静默改写 #160）；init_js 每次导航**重建 realm**
   （防上一文档的 window 状态泄漏进下一文档），依次注入 url/encoding/title/
   referrer/UA（HTTP 层与 JS 层必须一致——百度文库安全验证抓的就是这个
   错配）/语言/cookie/http_client/拦截器/DOM
2. **execute_scripts (367–750)**：脚本四分类（regular/defer/async/module）→
   外链并发抓取 `buffer_unordered(16)` → 软 deadline（脚本间检查）+ 硬
   watchdog（同步脚本风暴）→ readyState 时序（loading→interactive→complete，
   DCL/load 事件按 spec 顺序派发）→ preload 先于页面脚本（puppeteer
   exposeFunction 契约）→ data: URI 本地解码 → module 走 load_module →
   收尾 settle 循环（500ms 空闲×2 + 动态脚本挂起等待 a6bb741，OBSCURA_DYNAMIC_
   SCRIPT_SETTLE_MS）
3. **导航链 (752–971)**：navigate → navigate_with_wait(_post)（30s 总封顶
   OBSCURA_NAV_TIMEOUT_MS + 直达导航 referrer 清空）→ _inner（REDIRECT_LIMIT=10
   循环：单跳 → take_pending_navigation → SOP 门 → 链式 referrer 逐跳盖章 →
   下一跳）→ navigate_single (973–1260)（robots 检查 → about:/data:/POST/GET
   四分支 → charset 感知解码（Content-Type→meta sniff→UTF-8）→ parse_html →
   title → stylesheet 并发抓 16（print-only 跳过）→ init_js → CSS 注入
   `__obscura_css`（模板转义）→ iframe src 装载 → execute_scripts → DCL/
   Loaded/NetworkIdle 等待（NetworkIdle 500ms 静默窗+5s 上限+watchdog））
4. **settle 家族 (809–895)**：settle（一次性有界）/settle_until_idle（空闲
   返回 bool + storm 退避维护）/pump_event_loop_slice（会话后台泵：空则停
   泊等命令，风暴页 park 到退避期满）
5. **eval 家族 (1304–1424)**：evaluate（无 runtime 时 document.title/URL 静态
   降级）/evaluate_with_timeout/evaluate_for_cdp/call_function_on_for_cdp
   （错误降级为 undefined RemoteObjectInfo + warn 日志）
6. **CDP 辅助**：isolate_handle/cancel_v8_termination（进程级 V8 锁看门狗
   配对）/release_object(_group)/set_blocked_urls/set_intercept_tx/
   enable_intercept/take_pending_navigation/take_pending_binding_calls
7. **挂起恢复**：suspend_js（DOM 取回暂存，丢 realm）/resume_js（init_js 重建）
8. **历史**：push_history（连续去重+forward 截断）/set_history_index

### 4.4 环境变量清单（全部可运维调参）
`OBSCURA_NAV_TIMEOUT_MS`(30s) / `OBSCURA_SCRIPT_DEADLINE_MS`(10s) /
`OBSCURA_DYNAMIC_SCRIPT_SETTLE_MS`(3s) / `AGINXBROWSER_UA` /
`AGINXBROWSER_ACCEPT_LANGUAGE`。托管实例生产在用（unit 文件设了 UA 与
SCRIPT_DEADLINE=45s）。

## 5. 与上游 diff 量化（68 commits 分组）

### A 组：已吸收（wave 1–3 期间顺带完成）
edb1785（referrer 语义，wave 3）、a6bb741（动态脚本 settle，wave 3）、
f61493f（拒绝非 2xx 脚本响应——wave 3 的动态脚本版；**fetch 阶段的外链脚本
非 2xx 拒绝待确认是否同批**）、9209777 前身系列里的 charset/robots、#160/#139/
#33/#4 注释所引修复、c915a11（字符边界截断——b70e042 render 侧同型）。

### B 组：可吸收的同源修复/功能（Phase 1 主体，按性价比排序）
1. **fork_virtual_url.rs（43 行整文件）**——SPA click 后 `page.url()` 采纳
   bootstrap 的 `__virtualUrl`。**我们 bootstrap.js 已有 `__virtualUrl`（11 处
   引用），前端就绪，只差 Rust 侧 sync_virtual_url()**。性价比第一。
2. **profiles.rs（103 行整文件）**——UA/平台指纹池（Win/Mac × Chrome 143–146）
   + random_profile()：UA 与 navigator.platform/uaPlatform/version 全套一致。
   对 stealth 有直接增益；吸收时对接我们的 BrowserConfig。
3. **网络回调组**：on_request/on_response/off_*（RequestCallback/
   ResponseCallback 注册表）+ enable_interception + sync_js_network_events
   （d0b6fc4：JS 发起的请求进 Network 事件）+ StoredResponseBody/
   get_response_body/take_response_body_raw/alias_response_body/
   clear_response_bodies（1ca6047：大响应体流式取回）——服务层
   `/network` 类接口与 MCP 网络可见性可直接受益。
4. **导航超时组**：set_navigation_timeout/navigation_timeout（结构化字段 vs
   我们的环境变量——两者可并存）；evaluate_for_cdp_with_timeout/
   call_function_on_for_cdp_with_timeout；settle_for_duration；
   run_autonomous_event_loop_turn。
5. **零散**：add_preload_script（push 语义 vs 我们整组 set）；f4ebf3c obey-robots
   CLI 强制（服务层事项）；6b6ad93 SPA 路由上报（与 fork_virtual_url 同链路）；
   1f5bffc dead code 清理；409ac78 CDP browser context 隔离（对齐我们的
   session 模型时参考）。

### C 组：架构项（挂账，Phase 1 不动）
- **渲染浪潮（~25 commit，2026-07~08）**：screenshot 家族 7 方法（animation_
  time/sample/region）、prepare_screenshot_resources、DeviceMetricsBaseline、
  viewport/screen/device_scale_factor、LoadedStylesheet/StylesheetImport/
  AuthorStylesheetTarget、pdf.rs 整文件——全部服务于 obscura-render。**我们走
  Blitz 路线（src/screenshot.rs + blitz spike），战略分歧，不吸收**；但其
  「渲染感知的 Page 状态」设计在 Phase 2 渲染决策时要回头读。
- **frame realm（8 commit：6a4683d/3db9c60/bbc7d80/686544a/a954149/964bace/
  b5fa41b/13b7852）**：Page.frames: Vec<FrameRealm>、frame_urls/
  evaluate_in_frame、postMessage 跨 realm、子 frame 独立 V8 realm。与
  diting_js 挂账的 iframe realm 同一件事。**决策点：我们 iframe 目前是
  bootstrap 层 `_loadIframeSrc` 模拟 + 主 realm 执行；上游已走向真 realm
  隔离。等产品侧出现「iframe 内脚本污染主页面」或跨 frame 读取需求时再立
  项，单独立项不动 page.rs 主干。**
- 34373c3 import maps（diting_js 已挂账同款）。

### 我们有、上游（现版）未必有的
storm_backoff 退避、pump_event_loop_slice 会话泵、wait_for_cookie、
AGINXBROWSER_* 产品化环境变量、`__obscura_css` 注入时点（先于脚本）。
（storm 逻辑上游 page.rs 也有痕迹，Phase 1 diff 时逐 hunk 确认归属，防误删。）

## 6. Phase 1 认领建议（browser → diting_browser）

标准：该模块每一行都能说出为什么存在。切四批，每批独立可提交：

1. **批次 0（特征测试锁行为）✅ 2026-08-23**：`page.rs` 新增 tests 模块
   20 个测试（307→327 全绿）——5 个纯函数单测（subresource/SOP/referrer
   矩阵、data URI 解码、模板转义精确输出）+ 15 个行为测试（导航链落地/
   同源与跨源 referrer 逐跳/直达 referrer 空/SOP file 门/file: script src
   拦截/JS 链 10 跳上限 TooManyRedirects/DCL=脚本已执行/robots 拦与放行/
   NetworkEvent 三类与 request_id 格式/about:blank+preload/data: 文档/
   navigate_blank 复位/history 去重截断/suspend-resume DOM 保留+realm 重建）。
   基建：`local_http_server`（多路径、String body 支持跨源嵌端口）+
   `NetGuard`（锁+Drop 清环境变量）。
   **当场逮住并修复 1 个真 bug：robots.txt URL 丢端口**——`format!("{}://{}/
   robots.txt", scheme, host_str())` 里 host_str() 不含端口，非标端口站点
   （本地测试、内网服务）的 robots fetch 全部落空、缓存恒空、所有路径放行。
   修复：从完整 URL clone 派生（保留 scheme/host/port）。生产 80/443 站点
   恰好无感，属 6-19 快照里的潜伏 bug；上游现行代码已是同款 clone+set_path
   写法（page.rs:2812），我们独立收敛到同一修法。
2. **批次 1（吸收 B 组小件）✅ 2026-08-23**：307→337 测试。
   - **fork_virtual_url.rs**：整文件吸收（上游 fork commit d7dca7a，逐行同源）。
     调用点吸收上游同款：`process_pending_navigation` 的 else 分支——泵没有
     待处理导航时，页面自己 pushState 路由过也算导航（`sync_virtual_url()`
     采纳 `__virtualUrl` 进 `self.url` + history）。session.rs 的命令循环
     泵自动获得 SPA URL 跟随。
   - **hop-1 referrer**：`navigate_with_wait_post` 拆出私有
     `_ref(…, initial_referrer)` 变体（对齐上游 inner 的第 5 参）。
     直达自动化导航传 `""`（语义不变，测试锁定）；`process_pending_navigation`
     对页面自己发起的导航传 `navigation_referrer(source, target)`——此前泵
     路径第一跳 referrer 恒空，与上游语义对齐。
   - **profiles.rs**：指纹池 8 条（Win/Mac × Chrome 143–146）全量吸收 +
     `OBSCURA_PROFILE` 钉选 / `OBSCURA_ROTATE_PROFILE` 轮换。**默认钉
     `PROFILES[6]`（macOS Chrome 145）而非上游的 `PROFILES[0]`（Windows）**
     ——保持未配置部署一贯的人格，避免静默换脸。context 解析链变为：
     参数 → `AGINXBROWSER_UA` → `select_profile()`。**平台三元组字段
     （platform/ua_platform/ua_platform_version）不吸收**：我们 bootstrap.js
     的 `__obscuraPlatformFromUA` 直接从 UA 串推导 navigator.platform（百度
     文库修法），对任意 UA 覆盖（含 AGINXBROWSER_UA 自定义串）都一致；
     上游的显式全局量只覆盖池内 UA。
   - **add_preload_script**：push 语义（CDP addScriptToEvaluateOnNewDocument
     是追加制），与整组替换的 `set_preload_scripts` 并存。
   - **set_navigation_timeout/navigation_timeout**：Page 级结构化超时字段，
     None 时回落 `OBSCURA_NAV_TIMEOUT_MS`（默认 30s），与上游并存策略一致。
   - **明确不吸收**（记档）：`evaluate_for_cdp_with_timeout` /
     `call_function_on_for_cdp_with_timeout` / `settle_for_duration` /
     `run_autonomous_event_loop_turn`——服务层无消费者，我们的
     settle/settle_until_idle/pump_event_loop_slice 家族已覆盖同职能；
     CDP 层将来需要超时版 eval 时再回来拿。
   - 测试基建新坑记档：①`Page::evaluate`/`wrap_expression` 只包单表达式，
     多语句脚本会被包成 `return (a; b)` 语法错误、静默返回 Null——测试里
     拆开多次 evaluate；②`navigate()` 自带的 settle 窗口会追掉窗口内触发的
     timer 导航（inner 链直接消化），要测泵路径必须在 navigate 返回后再
     注入 timer。
3. **批次 2（网络回调组）✅ 2026-08-23**：337→344 测试。三件套按上游同
   源结构落进我们的模块划分：
   - **diting_net**：`CallbackRegistry`（on_request/on_response 注册表，
     upstream #408 逐行同源）+ `RequestInfo` + `ResourceType`（8 类 CDP 标签）
     + `fetch_with_callbacks`/`post_form_with_callbacks`（核心
     `fetch_with_method_traced`：每跳发送前 fire_request（带完整构造头）、
     终响应后 fire_response；`None` 走零开销原路径）。**未吸收上游
     `ResourceRequest`**（mode/credentials/max_response_bytes 的按资源整形）
     ——我们的 JS fetch op 自带 CORS/credentials 逻辑，net 层按资源整形是
     渲染浪潮的伴生架构，等有消费者再立。
   - **diting_js**：`op_fetch_url` 收尾三件事——响应体按
     `fetch-{N}` 存入 state（LRU：`OBSCURA_NETWORK_BODY_BUFFER_ENTRIES`=128
     / `_BYTES`=2MiB）+ `JsNetworkEvent` 入队（cap 4096，upstream #406）
     + fire 回调（JS 发起的流量也进页面观察者视野）。文本 Content-Type
     存 lossy-UTF-8，其余 base64。runtime 暴露 `set_callbacks`/
     `get_network_response_body`/`clear_network_response_bodies`/
     `take_js_network_events`。
   - **obscura_browser**：Page 持 `callbacks` + `response_bodies`（LRU 同
     上限）；`record_network_event` 改返 request_id，新增
     `_with_body` 变体（文档/脚本/样式表三类都存体，upstream #360）；
     `get_response_body`（页面侧 → JS 侧兜底）/`take_response_body_raw`
     （取走移交）/`alias_response_body`（loaderId 别名，#340）/
     `clear_response_bodies`（双侧清）；`sync_js_network_events`（drain
     JS 队列进 network_events，幂等）；`on_request`/`on_response`/
     `off_request`/`off_response`。文档/脚本/样式表 fetch 全部走
     `_with_callbacks`；stealth 文档 fetch 走 wreq 旁路（无回调钩子，
     观察者仍见脚本/样式/JS fetch）。
   - 测试 7 个：Document/Script/Stylesheet 三类回调齐发+请求头非空/
     off_request 双摘除可见/文档体存取+take 移交语义/alias 别名/
     octet-stream base64+字节精确回取/JS fetch→观察者+sync 后
     `fetch-{N}` 事件+体可解析+幂等 drain/clear 双侧清。测试基建新增
     `local_http_server_typed`（按路由 Content-Type，老 helper 委派它）。
   - 记档：**服务层尚无 network_events 消费者**——本批把内核管道铺通，
     `/network` 接口或 MCP 网络可见性是产品侧后续立项；届时消费点自行
     调 `sync_js_network_events`（pump 里不预调，避免无读者时白攒事件）。
4. **批次 3（读通 + 改名 + 摘 allow）✅ 2026-08-23**：344→343 测试（-1 是
   encoding.rs 重复 `#[test]` 合并，非删测）。四件事：
   - **摘 `#![allow(dead_code)]` 三处**（diting_browser/mod.rs、包装层
     src/page.rs、src/browser.rs），`cargo check`（bin）与 `--tests` 双模式
     全部清零警告。死项按三档处置，每处一行注释写明理由：
     ①**删**（全无消费者）：PageError::ParseError（零构造零匹配）、
     wrapper `wait_for_selector` + `Element::text/attribute`（服务层用
     原生 evaluate）、BrowserContext::with_proxy、Browser::new、
     diting_js `InterceptCallback` 类型别名（批次 2 被 CallbackRegistry
     取代的旧设计）、SearchEngine::js_extract_script（半拉子 feature，
     无引擎实现它）、encoding.rs 重复 `#[test]`。
     ②**cfg_attr(not(test), allow(dead_code))**（测试在用、bin 无消费者）：
     set_navigation_timeout / set_history_index / with_dom / 网络回调与
     响应体全家（批次 2 内核，/network 接口是待立消费者）/ preload 注册
     对 / suspend·resume·has_js / take_dom / runtime 侧 get·clear·take
     网络三件 / HTML_TO_MARKDOWN_JS / module_loader::new / with_options /
     with_full_options / RemoteObjectInfo 元数据字段 / firecrawl
     full_page（仅 screenshot feature 路径读）。
     ③**#[allow(dead_code)] + 注释**（上游 parity、消费者是未来的 CDP 层）：
     CDP 对象存储五件（store_object/store_object_with_meta/release_object/
     release_object_group/dom_ref）、isolate_handle+cancel_termination
     （per-command watchdog 管道，CDP server 本体未吸收）、
     call_function_on_for_cdp、set_blocked_urls、take_pending_binding_calls、
     set_intercept_tx+enable_intercept（Fetch 域拦截内核）、
     InterceptResolution::Fulfill/Fail（CDP 客户端才会发的线格式）、
     InterceptedRequest 字段、NetworkEvent 字段（/network 待立）、
     frame_id（frame realm 挂账）、captcha token/reason（求解器注入步）、
     BrowserProfile 平台三元组（persona 契约，bootstrap 运行时推导）、
     save_cookies（会话持久化立项再接）、BrowserBuilder user_agent/
     storage_dir。
   - **决策修正**：一度删掉 WaitUntil::NetworkIdle0/2 + from_str，读
     page.rs 时发现 navigate_single 的 network-idle 等待块（500ms 静默窗
     + 阈值 0/2，puppeteer 语义）在消费它们——变体与等待块是一体单元，
     恢复 from_str 并打 allow（firecrawl 兼容 waitUntil 入参是天然消费者）。
   - **wrapper 收敛**：goto 改走 `.navigate()`（Load 便捷入口转正）；
     browser.rs build() 的 if/else 坍缩成单调用。
   - **读通完成**：settle/settle_until_idle/pump_event_loop_slice（风暴
     退避 200ms→5s、热窗内 park 而非续喂）、charset 双路径（HTML
     Content-Type→meta 嗅探 1KB→UTF-8；CSS decode_non_html）、CSS 注入
     `__obscura_css` 的 U+2028/2029 转义、redirect 链 cross_scheme_to_file
     SOP 门——全部有理由能复述。
   - **改名**：`git mv` + 全仓 sed，`crate::obscura_browser` →
     `crate::diting_browser`，main.rs mod 声明与 RUST_LOG 过滤串同步；
     Page/BrowserContext 类型名保留（deno 无撞名）；JS 全局量
     `__obscura_*` 按计划不动（JS 面契约）。

Phase 1 完成（2026-08-23）：四个模块全部认领——每一行的存在理由都在本报告
与 js.md/net.md/dom.md 里。Phase 2（渲染决策：blitz vs 自研）现在有资格
开题——browser 认领的「渲染感知 Page 状态」一节正是那个决策的输入。

## 7. 已知风险与坑

- **Page 无锁单线程假设**：V8 进程级锁由 isolate_handle/cancel_termination
  配对守卫；改任何 eval 路径都要保住「watchdog arm → disarm」成对（js 认领
  期间多次踩过）。
- **execute_scripts 的双 deadline（软检查+硬 watchdog）缺一不可**：去掉软的
  会超时雪崩，去掉硬的会被同步风暴挂死——历史 bug 双向都发生过。
- **frame_id == id** 是 playwright 可见性契约，别"顺手规整"成自增帧号。
- **init_js 重建 realm 是安全边界**（上一文档状态泄漏），不是性能优化，
  不能改成复用 isolate。
- 测试基建复用 diting_js 的 PRIVATE_NET_ENV_LOCK + 本地 TcpListener HTTP
  服务器模式；注意本仓 test target 是 bin（`cargo test -- <filter>`）。
