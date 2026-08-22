# diting_net(现 obscura_net)摸底报告 — 认领 Phase 0

> 2026-08-22 摸底盘点。**2026-08-22 Phase 1 已完成**:B 组修复全部吸收(见文末认领建议的 ✅),模块已改名 `diting_net`,测试 42→60 个全绿。

## 一句话职责

HTTP 请求/响应的获取层:双客户端(reqwest 明路 + wreq stealth 指纹路)、Cookie 管理、编码检测、robots.txt 缓存、广告/追踪域名 blocklist、SSRF 防护(URL 校验 + DNS rebinding)。

## 体量与文件

| 文件 | 行数(我们/上游) | 职责 |
|---|---|---|
| `client.rs` | 651 / 2604 | `ObscuraHttpClient`(reqwest)、`Response`、`validate_url`(SSRF)、`fetch_file_url`、`derive_client_hints` |
| `wreq_client.rs` | 294 / 620 | `StealthHttpClient`(wreq)、TLS 指纹解析、EmulationOS 推导 |
| `cookies.rs` | 802 / 1204 | `CookieJar` + CookieEntry,set/get/持久化 |
| `encoding.rs` | 351 / 429 | encoding_rs 字符集检测(HTML5 顺序)+ URL 编码 |
| `robots.rs` | 163 / 172 | `RobotsCache` + robots.txt 解析 |
| `blocklist.rs` | 77 / 77 | pgl_domains.txt(3500+ 域)subdomain 匹配 |
| `mod.rs` | 21 / — | re-export |

共 2359 行,7 个文件,自带 42 个单元测试。逐行相同率:blocklist 100%、robots 100%、cookies 95%、encoding 93%、client 71%、wreq 50%。

## 核心数据结构

```
ObscuraHttpClient { client: reqwest::Client(OnceCell 惰性建), cookie_jar: Arc<CookieJar>, allow_private_network: bool }
StealthHttpClient { client: wreq::Client(Emulation = Chrome145 等), cookie_jar, extra_headers }
CookieJar { RwLock<HashMap<domain, HashMap<name, CookieEntry>>> }
CookieEntry { name, value, path, domain, secure, http_only, expires: Option<u64>, same_site }
RobotsCache { RwLock<HashMap<origin, RobotsRules>> }
```

关键设计决策:

- **双客户端**。明路 `ObscuraHttpClient` 用 reqwest(快、稳定),`StealthHttpClient` 用 wreq(Chrome TLS 指纹)。`src/search/*` 走明路,`obscura_browser` 可切 stealth。
- **无 gzip/brotli 解码(明路)**。reqwest 不启用 gzip/brotli feature——deliberate:百度在 gzip 被声明后会回 br。但 **stealth 路也没解码**(wreq_client.rs 全文无 Content-Encoding 处理),见坑 #2。
- **CookieJar key = (domain, name)**,`domain_matches` 手写无分配热路径。**path 参与存储但不参与 key**,见坑 #3。
- **SSRF 双层**:`validate_url`(前缀 IP 校验,拦 loopback/private/link-local/documentation/broadcast)+ `validate_url_rebinding`(resolve 后 IP 复检)。后者是 TOCTOU,见坑 #1。

## 服务层依赖点

| 消费方 | 用了什么 |
|---|---|
| `src/render.rs` | `ObscuraHttpClient.fetch`(截图渲染的主抓取路) |
| `src/screenshot.rs` | fetch + decode |
| `src/search/*`(mod/baidu/sogou/baidu_images) | `ObscuraHttpClient` 明路(不用 stealth) |
| `obscura_js/runtime.rs`、`ops.rs` | fetch/XHR 走 `ObscuraHttpClient` |
| `obscura_browser/page.rs`、`context.rs` | stealth client + CookieJar |
| `src/main.rs`、`src/cookie.rs` | CookieJar 持久化、`--allow-private-network` |

**对外接口面中等**——网络层被渲染、搜索、JS、浏览器四路同时消费。改名/重构爆炸半径比 dom 大,认领时所有消费方都要同步改。

## 已知坑(认领时要处理)

1. **stealth 路没有 SSRF 防护**。`wreq_client.rs` 的 `fetch()` 不调 `validate_url`——`StealthHttpClient` 可以直接打内网。上游现在的 `fetch_with_profile` 第一行就是 `validate_url(url, false)?` + blocklist 检查。**这是真洞**。
2. **stealth 路不解码 Content-Encoding**。上游 2026-08-04 专门加了 "decode Content-Encoding on the stealth client"(gzip 测试 `stealth_client_decodes_gzip_response`)。我们的 stealth 抓 gzip 页会拿到压缩字节。
3. **Cookie key 缺 path**。key 只有 `(domain, name)`,同 domain 同名不同 path 的 cookie 会互相覆盖。上游 2026-07-19 修 "key cookies by (name, path) not name alone"(#434/#438)。
4. **Cookie Path 语义不完整**。path 缺省取 `url.path()`(应为请求目录,RFC 6265 5.1.4),Path 匹配不按 `/` 边界(应为边界匹配,#383)。上游 2026-07-14/07-11 修。
5. **Set-Cookie Domain 不校验**。`Domain=` 属性盲信(cookie tossing 风险)。上游 2026-06-25 修 "validate Set-Cookie Domain against the origin"。
6. **`validate_url_rebinding` TOCTOU**。resolve 后复检 IP,但实际连接由 reqwest 重新 resolve——DNS 服务器两次返回不同结果即可绕过。上游换了方案:`SsrfGuardResolver` 自定义 Resolver 在连接时校验(根治)。我们的是自研,上游是更好的同源方案。
7. **meta charset 嗅探不挑来源**。上游 2026-08-05 修 "reject unrelated meta charset hints"(只认同文档的 meta,不认跨文档/无关 hint)。
8. **stealth SSL 环境变量不尊重**。`SSL_CERT_FILE`/`SSL_CERT_DIR` 空串被当路径、自定义 CA 根不加载。上游 2026-07-28~08-07 连修四轮(platform CA roots + 空 env 当未设置)。

## 上游这两个月改了什么(2026-06-19 内联至今,34 个 commit)

**A. 我们已经带的(内联时就有的或自研):** DNS rebinding 复检、blocklist、robots 缓存、`derive_client_hints`(sec-ch-ua 从 UA 推导,对应上游 06-23 f7b3ff5)、TLS 指纹可配置 + EmulationOS 从 UA 推导(自研,对应上游 06-22 4309935)。

**B. 同源 bug 修复,我们大概率还带洞(认领时逐条吸收):**

| 上游 commit | 内容 | 我们现状 |
|---|---|---|
| 06-25 f196b35 | Set-Cookie Domain 校验 | ❌ 带洞(坑 #5) |
| 06-23 768cbc0 | 非 stealth 导航 header 按 Chrome 顺序 | ⚠️ 未确认 |
| 07-11 d4fd288 | Cookie Path `/` 边界匹配 | ❌ 带洞(坑 #4) |
| 07-14 5ada3b4 | Cookie Path 缺省=请求目录 | ❌ 带洞(坑 #4) |
| 07-19 c8e482f | Cookie key 加 path | ❌ 带洞(坑 #3) |
| 07-28~08-07 | stealth 尊重 SSL_CERT_* + platform CA | ❌ 带洞(坑 #8) |
| 08-04 30a51b9 | stealth 解码 Content-Encoding | ❌ 带洞(坑 #2) |
| 08-05 a4e8ee5 | 拒绝无关 meta charset hint | ❌ 带洞(坑 #7) |
| 08-10 403a314 | GET 连接重置后重试 | ⚠️ 未吸收 |
| 08-10 582fb51 | 过期 cookie 更新时删除 | ⚠️ 未吸收 |
| 08-11 b79514b | Accept-Language 可覆盖 | ⚠️ 未吸收 |
| 08-19 92a70fe | CLI `--obey-robots` 强制 | ⚠️ 未吸收 |

**C. 大特性/架构,我们不跟(至少现在):**
- **`SsrfGuardResolver` 自定义 Resolver**(根治 TOCTOU,替代我们的 rebinding 复检)——这是"学上游更好的方案"而非"跟特性",**建议吸收**。
- `ResourceRequest`/`RequestMode`/CORS 校验机制 + page-owned subresource(2026-08-03~04,一堆 commit)——上游自研渲染引擎的子资源加载地基,我们渲染走 blitz,不跟。
- `CallbackRegistry` 被动 request/response 回调 + `interceptor.rs` 请求拦截 API(对应 CDP Fetch 域)——feature,暂不跟。
- fetch 客户端按 browser context 作用域(js 侧 07-26 ab6fa0e)。

## 认领建议(Phase 1 开工顺序)

1. **先补特征测试** ✅ 2026-08-22 已补 18 个(stealth SSRF/gzip、cookie (name,path) 并存/Path 边界/Domain 回退/host-only/过期删除、default_cookie_path、meta charset 来源、is_forbidden_ip、SsrfGuardResolver),60 个 net 测试全绿。顺手修了 `test_save_load_roundtrip` 缺 `#[test]` 从未运行的既存 bug。
2. **吸收 B 组** ✅ 2026-08-22 全部完成:
   - stealth 路补 `validate_url`(含重定向逐跳)+ file:// 处理(坑 #1)
   - wreq features 加 gzip/brotli/deflate/zstd → stealth 自动解码(坑 #2)
   - cookie key 改 (name,path)、`default_cookie_path`、`path_matches` / 边界、Domain 校验回退 host-only、过期更新=删除(坑 #3/#4/#5 + 上游 582fb51)
   - meta charset 改 `meta_attribute` tokenizer,拒绝无关 hint(坑 #7)
   - stealth 尊重 SSL_CERT_FILE/SSL_CERT_DIR(空串当未设置,坑 #8;reqwest 侧平台 CA 根未吸收,我们用不到私有 CA)
   - GET 连接重置重试(403a314)
   - Accept-Language 覆盖我们本就有(extra_headers),CLI `--obey-robots` 属于上游 cli crate,与我们无关
3. **吸收 `SsrfGuardResolver`** ✅ 2026-08-22 完成:`is_forbidden_ip` 统一 deny-set(补了 0.0.0.0/fc00::/7/IPv4-mapped),自定义 Resolver 连接时校验,**删掉了 TOCTOU 的 validate_url_rebinding**。只挂在直连 client 上——代理模式下代理解析目标 DNS,本地唯一解析是代理 host(常常是 127.0.0.1,不能拦)。**残留**:stealth(wreq)路没有自定义 resolver,rebinding 仍靠 validate_url 逐跳校验,wreq 侧 DNS 是内核解析,记为已知残留。
4. **改名 `diting_net`** ✅ 2026-08-22 完成:目录 git mv + 14 个消费方同步改;类型名一并去 obscura 前缀(`ObscuraHttpClient`→`HttpClient`、`ObscuraNetError`→`NetError`)。
5. **C 组挂账**:subresource/CORS 机制、interceptor、CallbackRegistry 记为"已知不跟",等渲染路线(Phase 2)再议。

**环境备忘**:本机 macOS 26 CLT 缺 C++ 头,`cargo check --features stealth` 需要 `CXXFLAGS="-isystem /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/c++/v1"`(boringssl 编译用)。
