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
| `bsky-post` | bsky.social | runs green | app password (`creds_json` vars) → createRecord → verify; page-context xrpc, no cookies — see flow.md |
| `doudian-login` | fxg.jinritemai.com (via open.snssdk.com) | runs green to QR handoff | direct SSO authorize URL: QR in ~1.8s vs 12-20s via the fxg front; human scans `qr_shot`, session lands logged-in — see flow.md |
| `taobao-login` | login.taobao.com | runs green to QR handoff | direct login.jhtml (3.7s vs 13.5s via homepage); QR canvas needs no click; cookie lands on .taobao.com — see flow.md |
| `taobao-live` | live.taobao.com | runs green logged-out | verdict-gated front probe: delivers "no wall + telemetry"; room list awaits engine hydration — see flow.md |
| `wechat-oa-post` | api.weixin.qq.com | runs green (certified OA) / draft-only (uncertified) | 5 pure http steps (token→cover→draft→publish→verify); account = `creds {app_id, app_secret}` via vars, never in flow.json — see flow.md |
| `xcom-profile` | x.com | runs green logged-out | needs foreign egress (`use_proxy: true` baked in) |
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
