<div align="center">

# ⚠️ 本项目已迁移

**AginxBrowser 已更名为 [diting](https://github.com/yinnho/diting)。**

同一引擎、同一团队、新名字。**所有活跃开发、发版与 issue 都在新址**——
本仓库冻结在 v0.6.4，仅作历史可读保留。

</div>

---

# → [github.com/yinnho/diting](https://github.com/yinnho/diting)

**为 agent 而生的浏览器引擎。** 一个 Rust 二进制，内置 V8，不依赖
Chromium——HTTP 是唯一接口面。本仓库拥有的一切，加上此后一年的引擎
进化：~25 MB 冷启动、自研 CSS/布局/绘制栈、每个响应自带回执、flow
重放、v0.8.x 发版线。

如果你 star 过本仓库，请移步 star
[diting](https://github.com/yinnho/diting)——下一个版本在那里发。

```bash
# 安装当前版本（来自新仓库）
curl -fsSL https://browser.aginx.net/install.sh | sh

# 或 Homebrew
brew tap yinnho/diting && brew install yinnho/diting/diting
```

## 迁移对照

| | 本仓库（aginxbrowser） | 新仓库（[diting](https://github.com/yinnho/diting)） |
|---|---|---|
| 最后版本 | v0.6.4（冻结） | [v0.8.x](https://github.com/yinnho/diting/releases/latest) |
| 二进制名 | `aginxbrowser` | `diting` |
| 配置目录 | `~/.aginxbrowser` | `~/.diting`（环境变量桥接兼容） |
| 环境变量前缀 | `AGINXBROWSER_*` | `DITING_*`（旧名仍接受） |
| HTTP API | 形状一致——迁移只需换进程名 | |

旧 v0.6.x release 资产与 `homebrew-aginxbrowser` tap 保留可用（存量
安装不受影响），但不再更新。

## 为什么改名

引擎长出了自己的身份：自研 V8 集成、自有运行时（`diting_runtime`）、
自研 CSS/布局/绘制栈——全程零 Chromium。新名字承载这个故事。

---

*[English migration notice](README.md)*

*历史描述（已被 [diting](https://github.com/yinnho/diting) 取代）：*
*> Agent 的浏览器。一个 Rust 二进制，内置 V8，无需 Chromium。*
*> 人有 Chrome，Agent 有 diting。*
