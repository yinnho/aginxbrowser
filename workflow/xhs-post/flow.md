# xhs-post — 小红书图文笔记发布 flow

> 2026-09-22 批175：加 verdict 门 + branch（issue #72/#73）。步 1.5 签名补丁后
> 插 verdict 步存 `v`，branch `v.verdict ∈ [login, challenge, captcha, empty]`
> 直接跳末尾 takeover throw——死 cookie 时的行为从「20s upload-content 空超时、
> 回执啥也不说」变成 6 步执行出带截图的接管回执：verdict=login（URL 规则表
> 命中 `/login`，facts 里 58 个请求、10 条 console 错误全带出），reason 直接
> 写着 cookie 死了走 import_curl。真机 receipts：旧行为无回执可言（超时即
> 全部证据），新回执 `/tmp/b175-xhs.json`（34.4s 里 20s 是步 3 的 ping-pong
> wait——那是分类前必要的双终态等待，verdict 之后一路秒断）。
> 步骤表里的「10 步」现在多了 verdict/branch/补集 branch/takeover/end 标记，
> 线性叙事不变，verdict 门插在步 3 之后。

创作平台页面自动化路线（与 15.9k★ 的 xpzouying/xiaohongshu-mcp 同路线：
个人号无官方 API，页面自身 JS 负责签名，驱动页面=零签名逆向）。选择器与
流程全部提炼自该项目 publish.go/publish-workflow.md（2026-09-14 调研，
只学不 fork），引擎跑的是自家 diting。

## 页面契约（上游提炼）

| 元素 | 选择器 | 说明 |
|---|---|---|
| 发布页 | `https://creator.xiaohongshu.com/publish/publish?source=official` | 401 未登录会规范跳 `/login` |
| Tab | `div.creator-tab`（文本=「上传图文」） | `div.upload-content` 可见为前置 |
| 挡路浮层 | `div.d-popover` | 上游阶梯 Esc→点空白→摘节点；flow 直接摘节点（必定生效） |
| 上传输入 | 首张 `.upload-input`，后续 `input[type=file]`（accept 含 image/） | 隐藏 input，JS 赋 files |
| 预览计数 | `.img-preview-area .pr` | ≥N 即上传完成（60s 窗） |
| 标题 | `div.d-input input` | 超长信号 `div.title-container div.max_suffix` |
| 正文 | `div[role="textbox"][contenteditable="true"]` → `div.tiptap[contenteditable="true"]` → `div.ql-editor` | TipTap 富文本 |
| 超长正文 | `div.edit-container div.length-error` | |
| 发布按钮 | 新版自定义元素 `xhs-publish-btn`（attr `is-publish`/`submit-disabled`）；旧版 `.publish-page-publish-btn button.bg-red` | |
| 成功判据 | URL 跳离 `/publish/publish`（15s） | 消除「点了按钮就算成功」的假阳性 |

## 步骤（10 步）

> 失败语义总则（2026-09-20 mock 全链实测教训）：**eval 的返回值只是遥测，
> `ok:false` 不会停流**——flow 会一路绿到 verify 才死在错的地方。要停在
> 正确的步、带回正确的原因，失败路径必须 `throw`（eval 异常 → 该步
> error → fail_receipt）。

1. **wait 双终态** — 等上传壳或 /login 二者先到。401→/login 是客户端
   跳转（水合后才发生），不先等会查到中间态。
2. **wall** — 登录墙遥测：eval 存 URL/need_login。**不带 expect**：
   登录页会按 lastUrl 弹回 publish 再被 401 弹回（乒乓），单点时刻的
   URL 检查恰好在弹回瞬间跑就假绿（2026-09-20 实测：op 时 URL=/login、
   expect 时已弹回 publish、最终又回 /login）。
3. **wait `div.upload-content`（硬门）** — 正向信号：壳渲染=已登录；
   超时回执带 URL（redirectReason=401）+ wall 遥测=登录墙的完整形状。
4. **tab** — 摘浮层 + 点「上传图文」，expect `input[type=file]`；
   找不到 tab → throw（回执 `tabs` 字段=诊断入口）。
5. **upload** — `const ARGS = {{args_json}}` 解包；逐张 data: URL fetch →
   blob → File（**不用 atob**——大 base64 展开爆栈，2026-09 实测教训），
   `input.files = File[]` + 手动派发 input/change（JS 赋值不自动派发，
   与 session_set_files 的 Rust 面同语义——批 #408 的 JS 面；2026-09-20
   合成页狗粮：2 文件全链绿，事件次序 input→change）。
   写 `window.__xhs_n` 供下一步引用。
6. **wait 预览计数** — predicate `.img-preview-area .pr` 数量 ≥ `__xhs_n`，60s。
7. **form** — 标题走 native setter + input/change（React value-tracker 绕过）；
   正文走 innerHTML 段落法：`<p>` 包裹 + 段间 `<p><br></p>` + input 事件
   （TipTap/ProseMirror 亲测法）；tags 以纯文本 `#tag` 追加末段；随后读
   超长信号（max_suffix/length-error），超长 → **throw**（回执点名真因，
   而不是死在下游的 submit-disabled）；末尾回点标题（上游稳定性怪癖）。
8. **wait 可点击发布按钮** — 表单填完后按钮解锁**有去抖**（上传/正文
   触发的校验重跑完才摘 submit-disabled）。predicate 同时扫新版
   `xhs-publish-btn`（跳过 `is-publish="false"` 克隆与 `submit-disabled="true"`）
   和旧版 `button.bg-red`（跳过 disabled/aria-disabled），10s——这是上游
   `waitForPublishButtonClickable` 轮询 15s 的等价物。选 Rust 侧 wait 而
   非 eval 内 setTimeout 循环：eval 挂起期间 timer 推进无把握，Rust 轮询
   每次 eval 驱动事件循环、天然推进 timer。
9. **publish** — 与上一步同一判据找可点击按钮（wait 过了却又锁上 →
   throw），scrollIntoView + click；回执带 `face`（widget/legacy）。
10. **verify** — wait predicate URL 离开 `/publish/publish`，15s。成功跳转
    = 已发布；超时=校验未过或被拦，回执带截图。

## 为什么上传不走 set_files step

flow 执行器的文本替换把数组型 var 变字符串（`Value::Array.to_string()`），
set_files 的 `files` 数组无法从 vars 结构化喂入；单张可用（`{{img0.b64}}`
整叶字符串），可变张数不行。eval + `input.files = File[]` 统一覆盖任意
张数，与 Rust 面同语义。——这是引擎面一个真实缺口（数组 var 无法整叶
进非 http step 的结构化参数），flow.md 记录在此，暂不改引擎。

## 登录配方（import_curl）

个人号无 API、无账密登录（扫码）→ 登录态一次性人工导入：

1. 本机 Chrome 开 `https://creator.xiaohongshu.com`，扫码登录。
2. DevTools → Network → 任一 creator.xiaohongshu.com 请求 → 右键
   **Copy as cURL**。
3. `POST /import/curl`，body 带 `account`（如 `xhs`）——会话落私有 cookie
   jar + persona 从真实 UA 播种（小红书风控绑指纹，别裸建会话）。
4. 之后每次 `POST /flow/run` 带 `session_id`（或先 `account_verify`
   验活）。

## 调用

```
POST /flow/run {"name": "xhs-post", "session_id": "<import_curl 会话>",
                "vars": {"args_json": "<下方对象 JSON>"}}
```

args_json 键（缺一跑前拒绝）：

- `title` — 标题（≤20 字，超长 flow 停在 form 步）
- `content` — 正文（`\n\n` 分段）
- `tags` — 字符串数组（≤10；v1 纯文本，见下）
- `images` — `[{name, content_type, content_base64}]`，至少 1 张；
  jpg/png/webp，本地文件 `base64 <file>` 现打

## 规矩（对齐 x/zhihu 台账）

- 发布是**不可逆外发动作**：真发前先跑一次到 form 步手动 inspect 回执
  （或临时把 publish 步删掉），确认无误再全量跑。
- verify 超时 ≠ 可重跑：先截图看页面上是什么（校验错/风控弹窗/网络失败），
  盲重跑可能双发。
- 选择器面随小红书前端更新会烂：回执 `tab`/`publish` 步的 `tabs`/`face`
  字段是诊断入口，按 publish.go 的维护法重抓选择器。
- 无登录态也能回归大半：mock 页（data: URL 复刻选择器契约 + 假按钮
  去抖/发布行为）跑全链验证执行器逻辑——2026-09-20 就是这法子暴露了
  ok:false 不停流和按钮去抖两个洞。mock 目录用完即删，不入库。
- 频率克制：新号连发是风控信号，间隔分钟级起步。

## v1 边界（不做）

- tags 为纯文本 `#tag`——不点联想下拉（`#creator-editor-topic-container`），
  不生成可点话题芯片；键盘驱动联想挂 v2。
- 不做：定时发布、可见范围、原创声明、商品绑定（上游全有实现，按需抄）。
- 不做：视频 tab、长文模式（长文流程更长：一键排版/模板选择/两段正文）。
