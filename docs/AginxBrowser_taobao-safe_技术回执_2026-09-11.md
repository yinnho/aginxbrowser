# AginxBrowser `taobao-safe` 技术回执

日期：2026-09-11  
对象：AginxBrowser 上游维护者  
范围：`aginxbrowser-0.4.0-taobao-safe` 的源码差异、构建方式、问题证据、复现建议与遗留事项

> 安全说明：本文不包含 Cookie、Token、账号、店铺、商品 ID、请求签名或其他凭证类信息。日志和 URL 均只保留诊断所需的域名与路径。

## 结论摘要

实际修复方向与上游提出的候选方向一致：页面脚本 `fetch()/XHR` 发出的 POST 经 rustls/reqwest 在**连接建立阶段**失败时，在明确开启环境开关且启用 `stealth` feature 的前提下，通过 wreq/BoringSSL 只回退发送一次 POST。

补丁明确限制为：

- 仅处理 `POST`；
- 仅处理 `reqwest::Error::is_connect() == true`；
- 必须显式设置 `AGINXBROWSER_STEALTH_POST_CONNECT_FALLBACK=1`；
- 回退只尝试一次，并禁用重定向；
- 请求体、必要请求头、凭证策略和响应 `Set-Cookie` 语义会被传递；
- URL 校验、私网限制与域名 blocklist 仍然生效；
- 发送阶段之后的错误、响应读取错误等不会触发 POST 重试，避免双提交。

该补丁在真实淘宝卖家后台流程中解决了原先的连接阶段失败，实际发布链路已成功完成。需要谨慎区分的是：**“rustls 连接阶段失败，而 wreq/BoringSSL 可以完成同一请求”已经得到验证；“淘宝目标服务器只支持 TLS 1.2 CBC，因此 rustls 必然失败”尚未被原始 TLS 告警或独立直连测试严格证明。**

## 一、源码与完整 diff

### 1.1 源码来源

构建源码树：

```text
/private/tmp/aginxbrowser-main
```

保留的上游源码归档：

```text
/private/tmp/aginxbrowser-main.tar.gz
来源：https://codeload.github.com/yinnho/aginxbrowser/tar.gz/refs/heads/main
SHA-256：4c15a1c1ef72037e474b819813121c3d657ec64ef9da4015376d37d10e76693d
```

该归档不包含 `.git`，因此以下信息**未保留/不可用**：

- `git log --oneline -10`
- `git status`
- 源码归档对应的精确 Git commit ID
- 相对 Git tag `v0.4.0` 的原生 `git diff`

不能凭记忆补造这些信息。安装的官方 0.4.0 二进制曾通过 `/health` 报告 commit `d8aeac0`，但本次修改使用的是当日下载的 `main` 分支归档。归档中的 `src/diting_js/ops.rs` 已包含少量相对 `d8aeac0` 更晚、且与本补丁无关的上游改动。因此，随附 diff 是**相对保留的原始 `main` 归档**生成的，不应被误称为严格的 `v0.4.0...HEAD` diff。

### 1.2 修改文件

将修改后的源码树与重新解压的同一份原始归档逐文件比较，排除 `target/` 后，仅有以下 3 个源码文件不同：

```text
src/diting_net/client.rs
src/diting_net/wreq_client.rs
src/diting_js/ops.rs
```

完整 unified diff：

```text
AginxBrowser_taobao-safe_v0.4.0.patch
423 行，19,483 字节
SHA-256：147ec12eae6512b6c6ac4919e38834c248136b65890e96042ff5a4d6cd95fe73
```

补丁生成方式等价于：

```bash
diff -u \
  <pristine-main>/src/diting_net/client.rs \
  /private/tmp/aginxbrowser-main/src/diting_net/client.rs

diff -u \
  <pristine-main>/src/diting_net/wreq_client.rs \
  /private/tmp/aginxbrowser-main/src/diting_net/wreq_client.rs

diff -u \
  <pristine-main>/src/diting_js/ops.rs \
  /private/tmp/aginxbrowser-main/src/diting_js/ops.rs
```

交付补丁中的路径已规范化为 `a/src/...` 与 `b/src/...`，便于上游审阅或择取实现。

### 1.3 每个文件的作用

#### `src/diting_net/client.rs`

新增 `HttpClient::scripted_post_connect_fallback(...)`：

- 检查显式环境开关；
- 重新执行 URL/私网安全校验；
- 仅在编译了 `stealth` 时取得 legacy transport；
- 同步 UA、Accept-Language 和上下文额外请求头；
- 调用 BoringSSL 传输层执行一次 POST；
- 回退失败时保留主传输错误并拼接 legacy transport 错误。

#### `src/diting_net/wreq_client.rs`

新增 `StealthHttpClient::post_scripted_once(...)`：

- 使用 wreq/BoringSSL 创建 POST；
- 不执行重定向，也不做内部重试；
- 传递脚本请求体；
- 请求局部 header 优先于上下文默认 header；
- 依据 `include_cookies` 决定是否携带当前 cookie jar；
- 接收并写回响应中的 `Set-Cookie`；
- 继续执行 URL、私网和 blocklist 校验。

#### `src/diting_js/ops.rs`

在 `op_fetch_url` 的 `req.send().await` 错误分支中增加严格门控：

```rust
current_method == reqwest::Method::POST
    && e.is_connect()
    && AGINXBROWSER_STEALTH_POST_CONNECT_FALLBACK == "1"
```

命中后重建脚本请求所需的 Origin、Referer、UA、Client Hints、Accept 和 Fetch Metadata，并连同当前请求体及 credentials 策略交给单次 BoringSSL POST。GET/HEAD 原有回退逻辑保持不变。

## 二、构建信息

### 2.1 构建命令

```bash
PATH=/private/tmp/cmake-pip/cmake/data/bin:/private/tmp/rust-local/bin:$PATH \
RUSTY_V8_ARCHIVE=/private/tmp/aginxbrowser-main/target/debug/gn_out/obj/librusty_v8.a \
CARGO_REGISTRIES_CRATES_IO_INDEX='sparse+https://rsproxy.cn/index/' \
CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
cargo build --release --features stealth
```

启用 feature：

```text
stealth
```

未启用 `screenshot`。Cargo fingerprint 显示 `features=["default","stealth"]`，而项目的 `default=[]`。

工具链：

```text
rustc 1.98.1 (48a229cea 2026-09-01)
cargo 1.98.1 (797e8a9bc 2026-08-05)
```

release 构建成功。构建末尾曾出现 `rust-objcopy` 因本机缺少 `libLLVM.dylib` 而无法进一步 strip debug info 的警告；该警告没有阻止可执行文件生成与运行。

### 2.2 二进制校验

```text
/Users/chennan/.local/bin/aginxbrowser-0.4.0-taobao-safe
SHA-256：7a5684f301d773794629e1068bce5828c8ea590d20917001751b0c7993392b13

/private/tmp/aginxbrowser-main/target/release/aginxbrowser
SHA-256：7a5684f301d773794629e1068bce5828c8ea590d20917001751b0c7993392b13

/opt/homebrew/bin/aginxbrowser（官方 0.4.0 对照）
SHA-256：7cdfa6f3f56b7481421530370b5f7c45644942af4f809536ac412033b21f99e5
```

部署后的实际进程命令为：

```text
/Users/chennan/.local/bin/aginxbrowser-0.4.0-taobao-safe
```

## 三、全部差异与运行时调参

### 3.1 源码差异

相对保留的原始源码归档：

- 只改了上述 3 个文件；
- 没有修改 `Cargo.toml` 或 `Cargo.lock`；
- 没有修改配置默认值；
- 没有修改限速逻辑；
- 没有修改原有 cookie jar 实现；
- 没有修改 GET/HEAD 既有回退条件；
- 新代码只为 POST 回退增加请求头、请求体和凭证策略的传递。

### 3.2 非源码运行时配置

LaunchAgent 中启用了本补丁开关：

```text
AGINXBROWSER_STEALTH_POST_CONNECT_FALLBACK=1
```

同时存在以下本地运行时覆盖，它们不是 taobao-safe 源码补丁，也不是默认值变更，但为完整审计一并披露：

```text
AGINXBROWSER_CACHE_TTL_SECS=0
AGINXBROWSER_DOMAIN_RATE_PER_MIN=10
AGINXBROWSER_SESSION_PAGE_LIMIT=0
AGINXBROWSER_STORE=1
```

## 四、补丁前的错误与定位证据

### 4.1 原始错误

官方 0.4.0 执行页面脚本 POST 时返回的原始外层错误为：

```text
Error: error sending request for url (https://item.upload.taobao.com/sell/ai/asyncOpt.htm?...)
```

已脱敏接口：

```text
POST https://item.upload.taobao.com/sell/ai/asyncOpt.htm
```

同类错误也出现在：

```text
POST https://item.upload.taobao.com/sell/ai/submit.htm
```

网络记录特征：

- status 为 `0`；
- 类型为 `Fetch`；
- HAR 中没有响应 body；
- HAR timings 的 `send`、`wait`、`receive` 均为 `-1`；
- 没有得到 HTTP 状态码。

这些证据支持“失败发生在请求发送/响应阶段之前，属于连接建立路径”。但原始日志只保留了 reqwest 外层 `error sending request`，没有记录完整的 rustls 内层 source chain 或 TLS alert，因此不能从当时的日志反推出一个已经严格验证的具体 cipher/alert。

### 4.2 对照请求

在相同登录状态与请求形状下，系统原生 `curl` 对该 POST 获得 HTTP 200 和成功 JSON，而官方 0.4.0 的页面脚本请求在连接阶段失败。这将问题范围收敛到 AginxBrowser 主传输路径，而不是业务参数本身。

### 4.3 TLS 补充测试

问题修复后，使用本机 OpenSSL 3.6.2 做了补充测试：

```text
TLS 1.2 + AES128-SHA：握手成功，证书验证通过
TLS 1.2 + ECDHE-RSA-AES128-GCM-SHA256：握手成功，证书验证通过
```

本机当时解析/连接到的是 `198.18.0.0/15` 范围内的合成地址，表明流量可能经过本地代理、VPN 或透明转发。该结果只能证明**当前本地网络路径**同时接受 CBC 与 GCM，不能证明淘宝源站是 CBC-only，也不能推翻运行时 rustls 与 BoringSSL 行为不同的事实。

因此推荐上游将根因表述为：

> 已确认：特定阿里系端点上的 scripted POST 经 reqwest/rustls 在连接阶段失败，而同一请求经 wreq/BoringSSL 可成功。CBC-only/握手兼容性是高度吻合的候选解释，但当前没有原始 TLS alert 或不经代理的 cipher matrix 证明它是唯一根因。

## 五、补丁后的对照证据

### 5.1 同一端点与请求形状

taobao-safe 运行同一个只读资格查询 POST：

```text
POST https://item.upload.taobao.com/sell/ai/asyncOpt.htm
```

结果：

```text
HTTP 200
业务响应 success=true
```

对应日志出现：

```text
connection-stage rustls failure for https://item.upload.taobao.com/sell/ai/asyncOpt.htm; sending one scripted POST via legacy TLS transport
```

这说明 `e.is_connect()` 门控确实命中，且 BoringSSL 单次回退完成了请求。

### 5.2 真实工作流

补丁运行期间，以下类型的 POST 均观察到相同回退日志并得到正常 HTTP/业务响应：

```text
item.upload.taobao.com/sell/v2/submit.htm
item.upload.taobao.com/sell/draftOp/add.json
item.upload.taobao.com/sell/v2/asyncOpt.htm
item.upload.taobao.com/sell/batch/image/detail
stream-upload.taobao.com/api/upload.api
```

真实商品发布最终进入成功页。这是端到端有效性证据，但不建议把线上发布当作唯一回归测试。

### 5.3 A/B 证据强度说明

现有对照为：

- 官方 0.4.0：同一域名、路径和请求形状失败；
- taobao-safe：同一域名、路径和请求形状成功；
- 补丁日志明确记录 rustls 连接错误后进入 legacy TLS POST。

这属于可信的 before/after 对照，但不是在完全隔离环境、同一时刻、固定网络和固定登录会话下反复执行的严格实验室 A/B。登录会话和网络路径可能随时间变化，不能声称“官方版本 100% 必失败、补丁版本 100% 必成功”。

## 六、最小复现建议

### 6.1 淘宝真实环境复现（需要登录态）

无需提供任何凭证给上游。登录卖家后台后，在 AginxBrowser 页面上下文执行以下形状的只读请求即可：

```javascript
await fetch('/sell/ai/asyncOpt.htm?optType=qualifyQueryAsyncOpt&catId=<category-id>', {
  method: 'POST',
  credentials: 'include',
  headers: {
    'Content-Type': 'application/x-www-form-urlencoded; charset=UTF-8'
  },
  body: ''
});
```

观测点：

- 官方 0.4.0 是否以 `error sending request` 失败且没有 HTTP 状态码；
- taobao-safe 是否记录 connection-stage fallback 并返回 HTTP 200；
- 服务端业务响应是否正常。

该路径依赖账号、网络与淘宝当前服务配置，不适合作为长期 CI 测试。

### 6.2 推荐的本地 mock 回归测试

建议上游建立一个受控 TLS 测试服务，模拟“rustls 主客户端握手失败、wreq/BoringSSL 可以建立连接”的条件，并让 POST 端点递增服务端计数器。至少覆盖：

1. 主传输返回 `is_connect()` 时，回退恰好执行一次；
2. 服务端计数器最终为 `1`，证明没有双提交；
3. 请求 body、Content-Type、Origin、Referer、Fetch Metadata 和 cookie 策略保持一致；
4. response/read error 不触发重试；
5. 环境开关关闭时保持旧行为；
6. 非 POST 不进入新增分支；
7. 重定向不会在回退层自动跟随；
8. URL 校验、SSRF 私网保护和 blocklist 继续生效；
9. fallback 自身失败时，同时保留主传输错误与 legacy transport 错误，便于诊断。

## 七、未尽事项与建议

1. **没有新增专用自动化测试。** 当前验证以真实淘宝只读请求和实际发布流程为主，上游合入前应补单元/集成测试。
2. **CBC-only 根因没有被严格证明。** 原始 rustls 内层错误链未被记录，且补充 OpenSSL 测试经过疑似代理路径。建议记录完整 `source()` 链与 TLS alert，并在可控网络下测试 cipher matrix。
3. **当前开关影响所有域名的 POST 连接错误。** 虽然门控已很窄，但上游可以考虑增加域名/策略级配置，或者把行为纳入明确的 transport policy。
4. **回退不跟随重定向。** 这是防重复提交的安全选择，但与主 fetch 路径可能存在行为差异；建议把 301/302/303/307/308 分别写入测试。
5. **诊断信息不足。** 对发送前失败，当前 Network/HAR 只记录 status 0、空响应和 `-1` timings，未暴露底层错误链，导致无法直接区分 DNS、TCP、代理、TLS protocol/cipher/certificate 等原因。
6. **阿里系 GET/资源请求也偶有 rustls 连接失败。** 既有 GET/HEAD fallback 已覆盖其中不少情况；该现象提示问题可能是更广泛的 TLS/代理兼容性，而不只是单个提交接口。
7. **构建基线不是可验证的 Git checkout。** 建议上游不要直接把整棵临时源码树视为 `v0.4.0`，而应在官方分支上审阅随附 3 文件补丁，或按相同安全门控重新实现。

## 八、建议的上游合入原则

建议保留以下安全不变量：

- 只在主传输明确报告连接阶段失败时考虑 POST 回退；
- 请求一旦可能已经发送，不重试；
- 回退最多一次；
- 默认关闭或由明确 transport policy 开启；
- 保留 URL/SSRF/blocklist 校验；
- 明确定义重定向行为；
- 测试服务端实际接收次数，不能只测试客户端返回值；
- 日志记录错误类别和链路选择，但绝不记录 Cookie、Authorization、请求签名或完整敏感 query。

在这些约束下，将能力收进上游后即可让本地 `taobao-safe` 分叉退役，切回官方二进制。

