# workflow/ — server-side flow assets

Each subdirectory is one named flow: `<name>/flow.json` (plus optional docs).
`flow_run` with `{"name": "<name>"}` picks it up from here — no rebuild, drop
a directory to deploy. Override the location with `AGINXBROWSER_WORKFLOW_DIR`.
A name is one path segment of lowercase/digits/dashes; anything else is
rejected before it touches the filesystem.

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
| `xcom-profile` | x.com | runs green logged-out | needs foreign egress (`use_proxy: true` baked in) |
| `juejin-post` | juejin.cn | runs green | post pages are SSR; list pages stall (see flow.md) |
| `zhihu-answer` | zhihu.com | 403 wall logged-out | compose with a logged-in session — see flow.md |
| `x-reply` | x.com | runs green logged-in | post/reply with full write-header set + auto verify step |
| `x-read` | x.com | runs green logged-in | user mode (profile+follow state) / tweet mode (thread+did-my-reply-land) |
| `x-search` | x.com | runs green logged-in | SearchTimeline, results sorted by views — campaign target discovery |
| `x-follow` | x.com | runs green logged-in | friendships create/destroy; response `following` echoes pre-action state |
| `x-notifs` | x.com | runs green logged-in | v2 URT notifications feed, read-only triage — the inbox half of the loop |

The `x-*` family shares one self-healing prefix (p256 / bearer / ctx+bind)
and composes: `x-search` finds a target → `x-read` resolves ids and follow
state → `x-reply` posts and self-verifies → `x-follow` if warranted. All of
them need a logged-in x.com session passed as `session_id`.

The honest rule the samples demonstrate: a flow either replays green or
fails with a receipt (failing step, reason, URL, screenshot, saved-so-far,
session left alive). Failed samples stay installed on purpose — their
flow.md documents the wall and the composition recipe around it.
