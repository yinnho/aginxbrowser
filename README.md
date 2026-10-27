<div align="center">

# ⚠️ This project has moved

**AginxBrowser is now [diting](https://github.com/yinnho/diting).**

Same engine, same team, new name. **All active development, releases, and
issues live at the new address** — this repository is frozen at v0.6.4 and
kept readable for history.

</div>

---

<div align="center">

[![diting stars](https://img.shields.io/github/stars/yinnho/diting?style=flat-square&labelColor=161b22)](https://github.com/yinnho/diting/stargazers)
[![diting release](https://img.shields.io/github/v/release/yinnho/diting?style=flat-square&labelColor=161b22)](https://github.com/yinnho/diting/releases/latest)

</div>

# → [github.com/yinnho/diting](https://github.com/yinnho/diting)

**The engine-first browser for AI agents.** One Rust binary, built-in V8,
no Chromium — HTTP is the only protocol face. Everything this repo was,
plus a year of engine work since: ~25 MB cold start, own CSS/layout/paint
stack, receipts on every response, flow replay, and a v0.8.x release line.

If you starred this repo, star [diting](https://github.com/yinnho/diting) —
that's where the next release lands.

```bash
# install the current release (from the new repo)
curl -fsSL https://browser.aginx.net/install.sh | sh

# or Homebrew
brew tap yinnho/diting && brew install yinnho/diting/diting
```

## Migration notes

| | here (aginxbrowser) | new ([diting](https://github.com/yinnho/diting)) |
|---|---|---|
| Last release | v0.6.4 (frozen) | [v0.8.x](https://github.com/yinnho/diting/releases/latest) |
| Binary name | `aginxbrowser` | `diting` |
| Config dir | `~/.aginxbrowser` | `~/.diting` (env bridge in place) |
| Env prefix | `AGINXBROWSER_*` | `DITING_*` (old names still accepted) |
| HTTP API | same shape — port your calls, only the process name changes |

Old v0.6.x release assets and the `homebrew-aginxbrowser` tap stay up so
existing installs keep working; they will not receive updates.

## Why the rename

The engine grew its own identity: own V8 integration, own runtime
(`diting_runtime`), own CSS/layout/paint stack — no Chromium anywhere.
The new name owns that story. Full continuity notes in the
[new repo's docs](https://github.com/yinnho/diting/tree/main/docs).

---

*The Chinese README carries the same migration notice:
[中文迁移说明](README.zh-CN.md).*

*Historical description, superseded by
[diting](https://github.com/yinnho/diting):*

> # AginxBrowser
>
> **The Browser for AI Agents.** See the live web. Read it. Act on it.
> Remember it. One Rust binary with built-in V8, no Chromium required.
>
> Humans have Chrome. Agents have diting.
