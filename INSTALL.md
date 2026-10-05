# aginxbrowser — agent 自装指南（本文件写给 AI agent 读）

用户会把这句话丢给你：「帮我安装 aginxbrowser：
https://raw.githubusercontent.com/yinnho/aginxbrowser/main/INSTALL.md」。
照下面做，全程不需要用户敲命令。

## 0. 先查是否已装

```bash
curl -s http://127.0.0.1:8089/health
```

`{"status":"ok","engine":"diting",...}` = 已在跑，跳到第 2 步。

## 1. 安装（三选一，按序尝试）

```bash
# macOS / Linux Homebrew（首选）
brew install yinnho/aginxbrowser/aginxbrowser

# 或预编译二进制：去 https://github.com/yinnho/aginxbrowser/releases
# 选对应平台（macOS arm64/x64、Linux、Windows），下载 -> 校验 sha256 -> 放进 PATH

# 或源码（要 Rust 工具链）
git clone https://github.com/yinnho/aginxbrowser && cd aginxbrowser && cargo build --release
```

装完自检（特性/字体/出口体检）：

```bash
aginxbrowser doctor
```

## 2. 启动并验证

```bash
aginxbrowser            # 监听 0.0.0.0:8089，前台进程，别后台丢着不管
curl -s http://127.0.0.1:8089/health        # 期待 status:"ok"
curl -s -X POST http://127.0.0.1:8089/fetch -H 'content-type: application/json' \
     -d '{"url":"https://example.com"}'     # 期待 markdown 正文——真验一发
```

两个都通才算装好；不通就带着报错全文回来找用户，别瞎猜。

## 3. 装完之后你会用到什么

- 读页/抓取 `POST /fetch {"url":...}`（markdown 出品）；截图 `POST /screenshot`；
  搜索 `POST /search {"q":...}`（带本地缓存）
- 交互浏览 `POST /session/create` → `state/click/input/eval`
- **服务端 flow**（`workflow/<name>/flow.json`，`POST /flow/run {"name":...}`）：
  现成能力流见仓库 `workflow/README.md` 的 routing 表——`web-read`
  （任意页正文）、`bilibili-search`、`xueqiu-quote/trending`、`v2ex-*`
  四件、`github-read`、`youtube-subtitles`
- 境外站（v2ex/youtube）要引擎带 `AGINXBROWSER_PROXY` 环境变量启动
  （如 `AGINXBROWSER_PROXY=socks5h://127.0.0.1:8800 aginxbrowser`）；
  国内站直连不受影响（按域名清单自动分流）

## 4. 边界

- 引擎不管业务逻辑：flow 是盘上的数据资产，改 flow 不用重编
- 不自动解验证码；登录墙走 QR 接管（`/live?session=<id>` 人肉过）
