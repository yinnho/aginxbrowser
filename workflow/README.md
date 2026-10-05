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
| `doudian-login` | fxg.jinritemai.com (via open.snssdk.com) | runs green to QR handoff | direct SSO authorize URL: QR in ~1.8s vs 12-20s via the fxg front; human scans `qr_shot`, session lands logged-in — see flow.md |
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

