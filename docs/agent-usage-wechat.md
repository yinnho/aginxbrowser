# 用 AginxBrowser 抓取微信公众号文章（推荐方式）

## 什么时候用 AginxBrowser 而不是公众号 OA API

抓微信公众号文章内容时，**优先用 AginxBrowser，不要用 `mcp_wechat_oa_get_article`**：

- **OA API 路线**（`mcp_wechat_oa_get_article`）：只能抓**自己公众号**的已发布文章，需要 `article_id` + 登录凭证，且要绕一圈找 article_id。
- **AginxBrowser 路线**：直接用文章 URL 抓，**任意公开公众号文章都能抓，免 cookie、免登录**。

**唯一规则**：只要拿到的是 `https://mp.weixin.qq.com/s/xxxxx` 这种文章链接，直接用 AginxBrowser 抓全文，**不要**去找 article_id、不要调 OA API。

## 调用方法

向 AginxBrowser 发 HTTP 请求（`POST /eval`），执行 JS 提取标题/作者/正文：

```
POST http://<AGINXBROWER_ADDR>/eval
Content-Type: application/json

{
  "url": "https://mp.weixin.qq.com/s/Qadi5AWsZ3cO7BTKvoVMmA",
  "wait_secs": 4,
  "script": "({title:document.querySelector('#activity-name')?.textContent?.trim(),author:document.querySelector('#js_name')?.textContent?.trim(),account:document.querySelector('.rich_media_meta_nickname a')?.textContent?.trim(),publishTime:document.querySelector('#publish_time')?.textContent?.trim(),body:document.querySelector('#js_content')?.innerText})"
}
```

- `<AGINXBROWER_ADDR>`：AginxBrowser 服务地址（同机部署为 `127.0.0.1:8089`，跨机为服务器 IP `106.75.32.216:8089`）。
- `wait_secs: 4`：等微信页面 JS 渲染完，必须给（微信正文是 JS 渲染的，不等会拿到空）。
- **不需要任何 cookie**：微信文章是公开页，AginxBrowser 的 stealth 指纹伪装已足够绕过风控。

## 返回结构

```json
{
  "url": "https://mp.weixin.qq.com/s/Qadi5AWsZ3cO7BTKvoVMmA",
  "result": {
    "title": "消失的"龙虾"热：OpenClaw访问量腰斩，QClaw暴跌99%",
    "author": "AI智能体自动化搭建",
    "account": "AI智能体自动化搭建",
    "publishTime": "2026年6月15日",
    "body": "最近我发现有意思的事，公众号文章写openclaw的少了..."
  }
}
```

`result.body` 就是完整正文（纯文本，可直接用于写作/总结）。

## 其他能力（按需）

AginxBrowser 还有两个端点，同样免 cookie：

- `POST /fetch` — 抓页面转 markdown/html/text，支持 `selector`（CSS 选择器）、`max_chars`（截断，默认 50000）
- `POST /click` — JS 点击元素

## 注意事项

1. **必须带 `wait_secs`**（建议 3-5 秒），否则微信 JS 没渲染完，`body` 为空。
2. URL 里的 `xxxxx`（如 `Qadi5AWsZ3cO7BTKvoVMmA`）是微信文章短链 token，**直接当 URL 用**，不需要解析成 article_id。
3. 如果正文很长（撑爆上下文），用 `/fetch` + `max_chars` 截断，或抓后在调用方自行截断。
4. 不要对同一 URL 短时间内重复抓（AginxBrowser 有 10 分钟缓存，命中缓存直接返回；但缓存外的高频请求可能触发风控）。
