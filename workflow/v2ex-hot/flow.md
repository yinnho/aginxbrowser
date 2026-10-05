# v2ex 家族 — 公开 API 四件（hot / node / topic / member）

| flow | 干什么 | vars |
|---|---|---|
| `v2ex-hot` | 热门帖全量（当前 ~150 条） | — |
| `v2ex-node` | 某节点最新帖 | `node`（默认 python）、`page` |
| `v2ex-topic` | 帖子详情 + 回复第一页 | `topic_id`（URL `/t/<id>`） |
| `v2ex-member` | 用户公开档 | `username` |

零登录零 key。全是对 `www.v2ex.com/api/*` 的纯 `http` 步——无会话依赖、
秒回（本机实测 <1s）。

## 两个前提（都真机踩过）

1. **v2ex.com 境内直连被重置**（v4/v6 都 reset）。引擎侧要挂
   `AGINXBROWSER_PROXY`（本机 `socks5h://127.0.0.1:8800`）。注意引擎现行
   语义：env 一挂，**flow 的 http 步全量走代理**（`exec_http_step` 把 env
   当 step 客户端的 context proxy）；雪球/B站这类国内站别用 http 步打，
   走页内 fetch（见 xueqiu-*）。
2. **User-Agent 必须朴素**（本家 `aginxbrowser-flow/1.0`）。引擎默认 UA 是
   Chrome 字样，配引擎 TLS 指纹在 Cloudflare 眼里=错配，直接 managed
   challenge（实测默认 UA 落 "Just a moment..."，朴素 UA 200）。同出口
   IP 下 curl 能过、引擎默认 UA 不能过——判据就是 UA/指纹一致性，不是 IP。

## v2ex 页面（非 API）现状

session 直接导航 `www.v2ex.com/t/<id>` 会停在 about:blank（CF 挑战页把
导航挡了）——**API 能过、页面过不了**，页面级读取需要 stealth 面（另案）。
数据需求四件 flow 已全覆盖。

## 对照 Agent-Reach

他们的 v2ex 渠道（Python urllib）有一个 TLS unexpected-EOF 的 curl 回退
分支——那是 Python 标准库的病，我们客户端没有，不需要抄。
