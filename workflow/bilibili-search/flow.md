# bilibili-search — B站搜索（免登录，无 412）

`POST /flow/run {"name":"bilibili-search","vars":{"keyword":"..."}}`。

## 结构（2 步）

```
create goto www.bilibili.com   ← 导航=cookie 预热（buvid3/buvid4 风控 cookie）
wait readyState complete
eval 页内 fetch api.bilibili.com/x/web-interface/search/all/v2
     → video 块（20 条）+ media_bangumi 块
```

## 为什么我们没有 412

Agent-Reach 的判决是「yt-dlp 被 B站风控 412 全配置封死（2026-06），换
bili-cli」——裸 HTTP 客户端指纹必被风控。我们的姿势不同：**会话页内
fetch 就是真浏览器上下文**（引擎指纹 + 预热 cookie + 同站 origin），
实测 code 0，连 wbi 签名都不需要（all/v2 从带 cookie 的页内 GET 直接过）。
这是「引擎本体 vs 上游 CLI」的分野样本：他们换备选，我们没有备选可换。

## 细节

- 同雪球三坑的前两坑：**不带自定义 header**（preflight 400）、**eval 传
  timeout_ms**（默认 5s 不够）。
- 标题字段带 `<em class="keyword">` 高亮 HTML，剥掉再交付。
- 数据门两条：`code!==0`（风控/墙）和 video 空都就地 throw 带 fact。
- 翻页：`page` var。视频详情端点顺手也验过（`view?bvid=` code 0），
  要做 bilibili-video flow 时两行 eval 的事。

## 对照

他们的 bili-cli 后端 2026-03 起停更（他们 doctor 里的 warn 文案自己说的）。
