# Stealth / TLS 对比页（有回执）

[English](stealth-tls-comparison.md) | [中文](stealth-tls-comparison.zh-CN.md)

> 快照：**2026-10-19 Asia/Shanghai** · AginxBrowser **0.6.0**（探针用本地 debug 二进制，`/health` commit `0e6f37d`；当时 tip 之后多为文档/bootstrap 拆分，TLS 形未变。默认 TLS 预设名 `chrome145`，舰队 UA Chrome/154。初测发现 stealth 路径 `Sec-Ch-Ua` 仍为 v=145——已在后续提交覆盖为跟 UA）  
> 原始回执：[`bench/stealth-tls-20261019/`](../bench/stealth-tls-20261019/) · 索引 [`INDEX.json`](../bench/stealth-tls-20261019/INDEX.json)

本页量的是**线上握手长什么样**，不是「无法检测」营销分。下表数字均可在回执里复核；没跑到的竞品标 **待测**，并附公开文档引用。

---

## 方法论

### 量什么

| 信号 | 来源 | 意义 |
|------|------|------|
| **TLS JA4** | [tls.peet.ws/api/all](https://tls.peet.ws/api/all) | ClientHello 形状（去 GREASE），比 JA3 稳 |
| **JA3 hash** | 同上 | 备查；GREASE 会漂 |
| **Akamai HTTP/2 fingerprint hash** | 同上 | SETTINGS / WINDOW_UPDATE / 伪头顺序 |
| **User-Agent** | peet + [httpbin.org/headers](https://httpbin.org/headers) | **不要**和 TLS 预设名混为一谈 |
| **HTTP 状态 / 挑战页** | community.cloudflare.com、blog.cloudflare.com | 是否出现 bot 挑战；**不是** Turnstile 通过率宣称 |
| **长登录 / Cookie** | 本轮不做 | 见账户/会话文档，不在本 bench 断言 |

### 引擎

| 引擎 | 跑法 | 备注 |
|------|------|------|
| **aginxbrowser 0.6.0** | 本机 `target/debug`（`/health`=`0e6f37d`，非 cellar Homebrew），`POST /fetch` + `render_tier:"auto"`，可选 `tls_fingerprint` | 必跑 |
| **curl** | macOS `/usr/bin/curl` | 无浏览器 TLS 伪装 |
| **Google Chrome 155 headless** | `--headless=new --dump-dom` | 同机真 Chromium 基线 |
| **Kitesurf / Lightpanda / Obscura** | **待测** | 本机无二进制 / 无 CF token — 见引用 |

### 边界（先读）

- **不等于「防检测」。** JA4 像 Chrome 族 ≠ 站点放过你。JS/canvas、IP ASN、行为、验证码是别的层（见 [ANTI_BOT.md](ANTI_BOT.md)）。
- **UA / TLS 预设名 / Client Hints 要对齐看。** 舰队 UA 默认 Chrome/154，TLS 预设名仍叫 `chrome145`（wreq-util 形）。把 `tls_fingerprint` 切到 `firefox133` / `safari18` 会改 JA4/H2，**本构建不会同步改 UA**。初测 receipt 里 httpbin 曾出现 **UA 154 + Sec-Ch-Ua v=145**（stealth 未覆盖 emulation 内嵌 CH）——见 `aginx-httpbin-headers-auto.raw.json`；代码已改为跟 UA 派生 CH。 复测见 [`aginx-httpbin-headers-auto-after-ch-fix.summary.json`](../bench/stealth-tls-20261019/aginx-httpbin-headers-auto-after-ch-fix.summary.json)（UA 与 CH 均为 154）。
- **碰到挑战 ≠ 过关。** 等到 `cf_clearance` 超时也会如实记。
- **不打商家/鉴权生产 API**（无千帆上架等）。
- 探针间隔约 1–1.5s；只用公开指纹页。

### 复现

```bash
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18089 aginxbrowser &
curl -sS http://127.0.0.1:18089/health   # 期望 "stealth": true, "tls": "chrome145"
curl -sS -X POST http://127.0.0.1:18089/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://tls.peet.ws/api/all","format":"text","max_chars":0,"render_tier":"auto"}'
```

仅用 `render_tier:"http"` 会让 peet JSON API 直接 500（「plain HTTP 不可服务」）——要用 `auto` / `browser` 才能走 stealth 传输层导航。

---

## 矩阵 A — TLS / HTTP2 指纹（`tls.peet.ws/api/all`）

| 引擎 / 配置 | JA4 | Akamai H2 hash | UA（缩） | 回执 |
|-------------|-----|----------------|----------|------|
| **aginx** 默认（`chrome145`） | `t13d1516h2_8daaf6152771_d8a2da3f94cd` | `52d84b11737d980aef856699f885ca86` | Chrome/154 macOS | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-default.fp.json) |
| **aginx** `tls_fingerprint=chrome145` | 同上 JA4 / H2 | 同上 | Chrome/154 | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-chrome145.fp.json) |
| **aginx** `tls_fingerprint=edge145` | 与 chrome145 同 JA4 / H2 | 同上 | Chrome/154 | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-edge145.fp.json) |
| **aginx** `tls_fingerprint=firefox133` | `t13d1716h2_5b57614c22b0_eeeea6562960` | `6ea73faa8fc5aac76bded7bd238f6433` | Chrome/154 *（UA 未切）* | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-firefox133.fp.json) |
| **aginx** `tls_fingerprint=safari18` | `t13d2014h2_a09f3c656075_14788d8d241b` | `d4a2dcbfde511b5040ed5a5190a8d78b` | Chrome/154 *（UA 未切）* | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-safari18.fp.json) |
| **Chrome 155 headless** | `t13d1517h2_8daaf6152771_cb7bf5808d99` | `52d84b11737d980aef856699f885ca86` | HeadlessChrome/155 | [fp](../bench/stealth-tls-20261019/chrome-peet.fp.json) |
| **curl** | `t13d497h2_0d8feac7bc37_7395dae3b2f3` | `64a832f547be33249bf4d33e8a46c5dc` | `curl/8.0-stealth-bench` | [fp](../bench/stealth-tls-20261019/curl-peet.fp.json) |
| Kitesurf | **待测** | — | — | 官方：不能用**真实 TLS 指纹**谈 bot 挑战 — [CF 文档](https://developers.cloudflare.com/browser-run/kitesurf/)、[发布博文](https://blog.cloudflare.com/kitesurf/) |
| Lightpanda | **待测** | — | — | 维护者：改 UA 挡不住 HTTP/TLS 指纹可区分性 — [issue #1436](https://github.com/lightpanda-io/browser/issues/1436)、[最佳实践](https://lightpanda.io/blog/posts/best-practices-for-using-lightpanda) |
| Obscura（独立） | **待测** | — | — | 本机未装 |

### 怎么读

1. **curl 一眼不是浏览器** — JA4 `t13d497…`，H2 也不同。
2. **aginx `chrome145` / `edge145` 与真 Chrome 155 同属 Chrome 族，且 Akamai H2 hash 相同**（`52d84b11…`）。JA4 并非与本机 Chrome 155 字节级一致（`1516` vs `1517`）——属版本/预设漂移，不是「同一二进制」。
3. **`tls_fingerprint` 真的改线**：Firefox / Safari 预设给出不同的 JA4 + H2。
4. **诚实缺口：** Firefox/Safari TLS 预设本轮仍带着 Chrome/154 UA。默认请用自洽组合（Chrome TLS + Chrome UA）。

httpbin 上回执可见 Chrome 形 `Sec-Ch-Ua` / `Sec-Fetch-*`（Client Hints 主版本 **145**，UA 字符串却是 **154**）——又一个一致性信号；见 [`aginx-httpbin-headers-auto.summary.json`](../bench/stealth-tls-20261019/aginx-httpbin-headers-auto.summary.json)。

---

## 矩阵 B — 挑战 / 可达（只读）

| 目标 | curl | aginxbrowser | 说明 |
|------|------|--------------|------|
| `https://example.com/` | 200 | 200（`tier=http`） | 冒烟 |
| `https://blog.cloudflare.com/kitesurf/` | 200 | 200（markdown，`tier=http`） | 本出口**两边都没挑战**——不能当 stealth 胜负 |
| `https://community.cloudflare.com/` | **403** “Just a moment…” | API 200，正文仍是「正在进行安全验证」；等 `cf_clearance` **25s 超时** | 硬挑战两边都过不了；本轮 **没有** 清掉 Turnstile |
| `https://browserleaks.com/tls` | 200 HTML | 200 markdown 壳 | 只证明可达；完整密码套件 UI 偏 JS |

---

## 竞品公开说法（待测 — 只引用）

### Cloudflare Kitesurf

[浏览器文档 — What Kitesurf cannot do yet](https://developers.cloudflare.com/browser-run/kitesurf/)：

> Negotiate a bot-challenge handshake with **real TLS fingerprints**.  
> Start a long-running, authenticated session that requires persistent state.

[发布博文](https://blog.cloudflare.com/kitesurf/) 同款表述。本页对准的就是这条能力差：AginxBrowser stealth 构建**会**在线上打出命名浏览器 TLS 预设（矩阵 A）；我们**不**宣称通杀挑战（矩阵 B）。

### Lightpanda

倾向自我标识，[拒绝 Mozilla UA 冒充](https://lightpanda.io/blog/posts/best-practices-for-using-lightpanda)。维护者称改 UA 解决不了 Web API 或 **HTTP/TLS 指纹**可区分性（[#1436](https://github.com/lightpanda-io/browser/issues/1436)）。装上二进制复跑 peet 前保持 **待测**。

### Obscura

本机无独立安装。AginxBrowser 的 `diting` 与 obscura/blitz 谱系相关，但 stealth HTTP/TLS 是自有路径——不要默认上游 Obscura 的 JA4 等于上表。

---

## 尚未测量

- 同网同机对 **Kitesurf / Lightpanda / Obscura** 的 peet.ws JA4 并排
- 固定语料上的 Turnstile / managed-challenge **通过率**
- **长登录** Cookie 存活（accounts / `import_curl`）——产品文档有写，本 bench 未测
- AginxBrowser 在 `chrome145` 之后预设演进时的 JA4 漂移
- Client Hints（`Sec-Ch-Ua` v=145）与舰队 UA Chrome/154 的对齐

---

## 相关

- [ANTI_BOT.md](ANTI_BOT.md) — 反爬分层与站点实战  
- [API.zh-CN.md — `tls_fingerprint`](API.zh-CN.md) — 预设名  
- [README.zh-CN — 真实 TLS 指纹](../README.zh-CN.md)
