# xueqiu-trending — 雪球热股榜 + 热帖时间线（免登录）

`POST /flow/run {"name":"xueqiu-trending"}` 一发回执：人气榜热股
（vars `size` 默认 10，type=10 人气榜）+ 公共时间线热帖（vars `posts`
默认 20，上限 50）。与 xueqiu-quote 同机制（cookie 预热 → 页内 fetch），
两端点串在一个 eval 里拿。

## 坑（补 quote 之外的两条）

1. **热帖端点必须 `www.xueqiu.com`**：裸 `xueqiu.com/v4/...` 会 301 到
   www，而那个 301 响应的 `Access-Control-Allow-Origin` 是空串，浏览器
   上下文直接拦（"redirect blocked: Origin not in ACAO ''"）。
   Agent-Reach 的 Python 版没有 CORS 概念跟着重定向就过去了——浏览器
   语义下必须直连 www。
2. **热帖每条的 `data` 字段是 JSON 字符串**，要二次 parse；`text` 是带
   标签的 HTML，剥标签 + 压空白再截 200 字。
3. 榜单热股同理走 `stock.xueqiu.com`（跨子域但对方开了 CORS 面，裸 GET
   + credentials 是 simple request，不预检）。

## 数据门

双端点**全空**才 throw（单边空不扣绿——行情侧和社区侧任一活着回执就有用）。
