# 把 AginxBrowser 当 agent-browser 风格引擎用（HTTP）

> 状态：**今天用 HTTP 即可**（`/session` + `/snapshot` + `ref`）。原生 `vercel-labs/agent-browser --engine aginx` 需要 CDP `serve`，见 [`cdp-serve-design.md`](cdp-serve-design.md)。不做 MCP。

[English](agent-browser-engine.md)

## 为什么先走 HTTP

[vercel-labs/agent-browser](https://github.com/vercel-labs/agent-browser) 接 Lightpanda / Obscura 的方式是拉起 **CDP** 进程（`obscura serve --host 127.0.0.1 --port …`）。AginxBrowser 当前对外是 **本地 HTTP API**，agent 循环一样：

1. 建 session → 导航
2. **snapshot** 可交互元素（`ref=eN`）
3. 用 `ref` **click** / **input**
4. close

CDP 落地前，编码类 agent 请直接打 HTTP（或用下面的薄 shim）。今天就能复制粘贴；上游 `--engine aginx` PR 卡在 CDP，标注为 pending。

## 前置

```bash
brew install yinnho/aginxbrowser/aginxbrowser   # 或见 docs/install.zh-CN.md

aginxbrowser &
curl -sS http://127.0.0.1:8089/health
```

基址可用 `AGINXBROWSER_URL` 覆盖。

## 三命令冒烟

```bash
export BASE="${AGINXBROWSER_URL:-http://127.0.0.1:8089}"

SID=$(curl -sS -X POST "$BASE/session/create" \
  -H 'content-type: application/json' \
  -d '{"url":"https://example.com"}' | jq -r .session_id)

curl -sS "$BASE/session/$SID/snapshot?format=text"

curl -sS -X POST "$BASE/session/$SID/click" \
  -H 'content-type: application/json' \
  -d '{"ref":"e0"}' | jq .
```

仓库内 shim：

```bash
./scripts/aginx-agent open https://example.com
./scripts/aginx-agent snapshot
./scripts/aginx-agent click e0
```

## HTTP 对照表

| 类 agent-browser 动词 | AginxBrowser HTTP |
|----------------------|-------------------|
| open / navigate | `POST /session/create` `{url}` 或 `POST /session/{id}/navigate` |
| snapshot | `GET\|POST /session/{id}/snapshot` |
| click `@eN` | `POST /session/{id}/click` `{"ref":"eN"}` |
| fill / type | `POST /session/{id}/input` `{"ref":"eN","text":"..."}` |
| eval | `POST /session/{id}/eval` |
| close | `POST /session/{id}/close` |

`ref` 是当前 snapshot 的 `e` + index；导航或 DOM 变了要重新 snapshot。

完整文档：[`API.zh-CN.md`](API.zh-CN.md)。

## Skill

仓库根 `SKILL.md`：多步 UI 优先 `/snapshot` + `ref`；单页阅读仍用 `/fetch`。

## 和 vercel agent-browser 的关系

1. **推荐**：agent 直接打 Aginx HTTP（本文 + `scripts/aginx-agent`）。
2. **并存**：CDP 引擎继续用 `agent-browser --engine chrome|lightpanda|obscura`；需要 stealth/TLS 或国内站会话时用 Aginx。
3. **未来**：[`cdp-serve-design.md`](cdp-serve-design.md) 落地后，上游可仿 Obscura 适配器。**在此之前上游 PR pending。**

## 相关

- [`stealth-tls-comparison.zh-CN.md`](stealth-tls-comparison.zh-CN.md)
- [`cdp-serve-design.md`](cdp-serve-design.md)
