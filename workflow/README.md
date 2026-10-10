# workflow/ — server-side flow assets

Each subdirectory is one named flow: `<name>/flow.json` (plus optional docs).
These are data assets deployed beside the binary — the engine carries none of
them inside the exe (browser stays out of business logic). At runtime a
`<name>/flow.json` under this directory (or `AGINXBROWSER_WORKFLOW_DIR`) is
what `flow_run(name=…)` loads — drop a directory in to deploy, no rebuild.
The release tarball ships this directory as files so a fresh unpack has the
samples; brew/agpkg installs ship the bare binary (grab `workflow/` from the
repo when needed). A name is one path segment of lowercase/digits/dashes;
anything else is rejected before it touches the filesystem.

## The working mode (read this first, agent)

You are a delegator, not a browser operator. The engine + this directory
is the deliverable: **aginxbrowser + flow = a working capability**, and
your interface is `POST /flow/run {"name", "vars", "session_id"}` — the
receipt is the function's return value (`status` / `saved` / `session_id`).

1. **Before anything else, look for an existing flow**: the tables below
   map intent → flow name; `POST /flow/search` lists what the configured
   hub can install. Compose login state via `session_id`.
2. **Only if nothing covers the task**, mine it by hand: drive a session
   (`/session/*` — create, wait, click, eval). Micro-operations are the
   discovery scaffold, not the way to keep working — every quirk you hit
   (anti-bot walls, widget shapes, timing traps) is knowledge the next
   agent should inherit for free.
3. **The moment the path runs green, distill it**: export the recording,
   curate it, land `workflow/<name>/flow.json` + a `flow.md` that says the
   run recipe, the exact args contract, and the green receipts (dates +
   evidence). An unrecorded success is a debt — the next agent re-mines
   what you already paid for.
4. **A failed flow is data, not a fallback trigger**: the receipt names
   the failing step, reason, URL, screenshot, and leaves the session alive
   — repair the flow (or take the session over once to diagnose), then
   re-run it. Hand-driving as the routine means the knowledge never lands.

## Authoring loop

1. Drive a session by hand (HTTP `/session/*` or the MCP `session_*` tools):
   create → wait → click/input/scroll → eval.
2. `GET /session/:id/export?format=json` — the recording as a flow document.
   Cookies/storage are stripped (a flow is a shareable asset); `ok:false`
   probe actions are dropped.
3. Curate: prune probe evals, parameterize URLs into `{{vars}}`, add `wait`
   steps before each read (waits are not recorded — the recorder only logs
   actions), add `expect` assertions, mark extraction evals with `save`.
4. Replay: `POST /flow/run {"name": ...}` — receipt carries `status`, `saved`
   outputs, and the live `session_id`.

## Installed samples

| name | site | state | notes |
|---|---|---|---|
| `bilibili-search` | bilibili.com | runs green | cookie 预热 → 页内 fetch 搜索 API：真浏览器上下文无 412 无 wbi（yt-dlp 被封的那条路我们不走）；video+bangumi 块 — see flow.md |
| `bsky-post` | bsky.social | runs green | app password (`creds_json` vars) → createRecord → verify; page-context xrpc, no cookies — see flow.md |
| `doudian-login` | fxg.jinritemai.com | runs green to QR handoff | 页内切换器出码（10-06 翻案：全表单面等齐+真鼠标链，313ms 翻转）；`qr_data` 直递，码 60-100s 寿命、递码要一气呵成；干净登录开新 account（共享 jar 会被旧 cookie 静默 302）— see flow.md |
| `doudian-publish` | fxg.jinritemai.com | runs green (draft lands) | 纯 API 三发链：getSchema(空)→getSchema(占位 spec 让服务器 canonicalize)→addWithSchema 草稿（提交步 eval 内 await，#240 工作区）；**getSchema 双步同款 eval-await**（10-10，boot 风暴续体丢三现后根治，链 190s→57s）；step1 args 前置校验（标题汉字当量 ≤30 等，2s 拦死不烧整链）；**规格值图=值对象 `img_url`**（6019 强制图品类 18/18 canonicalize 回显，`spec_images[]` 平行数组进 args）；组合 doudian-login 的 `session_id` + 全量 `args`（契约见 flow.md）；带图 = 先 `doudian-upload` 出 URL 填 `main_images`/`spec_images`（10-09/10-10 五绿单）— see flow.md |
| `doudian-upload` | fxg.jinritemai.com | runs green | 页内 fetch 直打 `/product/img/batchupload` 回 CDN URL（绕开 widget：快喂窗口+不回填两坑）；**每图一 eval 步**（chunk 版两连死于 #240 face-2：净层 6×200 续体全丢）+ boot 哨兵门；断点续传=回执 `img_i` 喂 `vars.done` 重跑（10-10 双绿单）；compose 登录 `session_id`，产出喂 doudian-publish 的 `main_images`/`portrait_images`/规格图 — see flow.md |
| `github-read` | api.github.com | runs green | 免 gh/token 公开读：repo 元数据 + issue 列表（search API 避开 PR 混排）；404/403 落 fail 步带原始 body — see flow.md |
| `taobao-login` | login.taobao.com | runs green to QR handoff | direct login.jhtml (3.7s vs 13.5s via homepage); QR canvas needs no click; cookie lands on .taobao.com — see flow.md |
| `taobao-live` | live.taobao.com | runs green logged-out | verdict-gated front probe: delivers "no wall + telemetry"; room list awaits engine hydration — see flow.md |
| `taobao-shop-collect` | shop<N>.taobao.com | runs green (reboot handles boot roulette; wall receipt verified ×3) | buyer-side listing collect: 60 cards via React fiber walk + secfont price tokens/cps (offline decode companion in flow.md); data-level gates, no verdict (session-cumulative rows false-wall a rebooted-clean page) — see flow.md |
| `v2ex-hot` / `v2ex-node` / `v2ex-topic` / `v2ex-member` | www.v2ex.com/api | runs green (needs `AGINXBROWSER_PROXY` egress + plain UA) | 公开 API 四件套纯 http 步；朴素 UA 过 CF（默认 Chrome UA 配引擎 TLS 指纹=错配会被 managed challenge）——机制与墙见 v2ex-hot/flow.md |
| `web-read` | any URL | runs green (CF-challenge sites excluded) | 引擎本体=reader 的 Jina 平替：goto + 容器优先级抽取 + 截断；404 页诚实抽取 — see flow.md |
| `wechat-oa-post` | api.weixin.qq.com | runs green (certified OA) / draft-only (uncertified) | 5 pure http steps (token→cover→draft→publish→verify); account = `creds {app_id, app_secret}` via vars, never in flow.json — see flow.md |
| `xcom-profile` | x.com | runs green logged-out | needs foreign egress (`use_proxy: true` baked in) |
| `xueqiu-quote` / `xueqiu-trending` | xueqiu.com | runs green | 页内 fetch（flow http 步无 cookie 带不了雪球会话）：行情/热股榜+热帖；三坑=自定义 header 触发 preflight 400、eval 5s 预算不够、热帖必须 www 直连 — see flow.md |
| `youtube-subtitles` | youtube.com | runs green (use_proxy baked in) | 元数据+字幕轨全表；**正文=pot 墙原料交付**（timedtext 200 空 body，四路实测全墙）— see flow.md |
| `juejin-post` | juejin.cn | runs green | read a post page; list pages stall (see flow.md) |
| `juejin-publish` | juejin.cn | needs a logged-in session | draft then publish; no proxy; sample title is refused |
| `zhihu-answer` | zhihu.com | 403 wall logged-out | compose with a logged-in session — see flow.md |
| `x-reply` | x.com | runs green logged-in | post/reply with full write-header set + auto verify step |
| `x-read` | x.com | runs green logged-in | user mode (profile+follow state) / tweet mode (thread+did-my-reply-land) |
| `x-search` | x.com | runs green logged-in | SearchTimeline, results sorted by views — campaign target discovery |
| `x-follow` | x.com | runs green logged-in | friendships create/destroy; response `following` echoes pre-action state |
| `x-notifs` | x.com | runs green logged-in | v2 URT notifications feed, read-only triage — the inbox half of the loop |
| `xhs-post` | xiaohongshu.com | login wall logged-out | creator-platform page automation (no API for personal accounts) — login via `import_curl` + `account`, see flow.md |

Account state enters a flow one of three ways — each flow.md says which it
takes. A logged-in `session_id` reused at run time (the `x-*` family;
`xhs-post` gets there via `import_curl` or the account wizard), or
credentials passed as runtime `vars` by the caller (`wechat-oa-post` takes
a `creds {app_id, app_secret}` object, `bsky-post` a `creds_json` app
password), read from a local file on the caller's side — secret values
never live in flow.json or this repo.

The `x-*` family shares one self-healing prefix (p256 / bearer / ctx+bind)
and composes: `x-search` finds a target → `x-read` resolves ids and follow
state → `x-reply` posts and self-verifies → `x-follow` if warranted. All of
them need a logged-in x.com session passed as `session_id`.

The honest rule the samples demonstrate: a flow either replays green or
fails with a receipt (failing step, reason, URL, screenshot, saved-so-far,
session left alive). Failed samples stay installed on purpose — their
flow.md documents the wall and the composition recipe around it.

## Capability flows — routing (agent-facing)

读/搜类零登录流，agent 按意图直达：

| 意图 | flow |
|---|---|
| 读任意网页正文 | `web-read`（`vars.url`） |
| 搜B站视频 | `bilibili-search`（`vars.keyword`） |
| 股票行情 / 热股热帖 | `xueqiu-quote`（`vars.symbol`）/ `xueqiu-trending` |
| V2EX 热帖/节点/帖详情/用户 | `v2ex-hot` / `v2ex-node` / `v2ex-topic` / `v2ex-member` |
| GitHub 仓库/issue 公开读 | `github-read`（`vars.repo`） |
| YouTube 视频信息+字幕轨 | `youtube-subtitles`（`vars.url`） |

境外站（v2ex / youtube）要引擎挂着 `AGINXBROWSER_PROXY`；国内站直连。
通用搜索走引擎搜索层（`/search`），不是 flow。

