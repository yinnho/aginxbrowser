# xueqiu-quote — 雪球个股实时行情（免登录）

`POST /flow/run {"name":"xueqiu-quote","vars":{"symbol":"SH600519"}}` 一发回执：
现价/涨跌幅/高低开收/量额/市值/PE/PB。symbol 支持 SH 沪、SZ 深、港（如 00700）、
美股（如 AAPL）。零登录、零 key。

## 结构（2 步）

```
create goto www.xueqiu.com   ← 这跳导航就是 cookie 预热（acw_tc 等）
wait readyState complete     ← SPA 首页重，等 complete 即可，不等业务 selector
eval 页内 fetch v5 行情       ← credentials:include 带会话 cookie
```

## 为什么是「页内 fetch」而不是 flow http 步

雪球行情接口要会话 cookie；flow 的 `http` 步是每步新建的**无 cookie 裸客户端**
（引擎设计：凭证不进 step，也不跨步累积）。页内 fetch 走会话自己的网络栈，
cookie 自动带、TLS 指纹是真浏览器。雪球在 BLOCKED_DOMAINS 之外，会话流量
按需直连，不碰代理。

## 三个踩坑（真机实测）

1. **页内 fetch 不能带任何自定义 header**——加 `Referer` 就从 simple request
   变成要预检的请求，雪球对 OPTIONS 直接 400。
2. **eval 默认 5s 预算不够**：首跳含 DNS/TLS/cookie 握手实测 4.8s+，步里必须
   传 `timeout_ms`（本流 25s）。
3. 数据门内嵌：`q.symbol` 缺失就地 throw（http 非 200 / code 非 0 / symbol
   打错都会走到这），回执带原始响应前 200 字。
