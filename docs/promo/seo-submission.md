# SEO 收录提交清单

> 一次性任务：把 browser.aginx.net 提交给 Google / 百度 / Bing 搜索引擎。
> 站点基础设施已就绪：`robots.txt`（Allow all + sitemap 指针）、`sitemap.xml`（7 URL + hreflang 双语）、静态优先 nginx（验证文件放 `web/` 直接可访问）。

## 已完成

- [x] sitemap.xml 上线（7 URL，中英 hreflang）
- [x] robots.txt 上线（允许抓取 + Sitemap 声明）
- [x] nginx 静态优先：验证文件（googleXXX.html 等）放 `web/` 即自动可访问，无需额外配置
- [x] Bing sitemap ping —— **已废弃**（410），只能走 Webmaster Tools 账号

## Google Search Console（建议 DNS 验证，最省事）

1. 登录 [search.google.com/search-console](https://search.google.com/search-console)
2. 「添加资源」→ 域名属性 → 填 `browser.aginx.net` → 得到一条 **TXT 记录**
3. 把 TXT 加到域名 DNS（browser.aginx.net 的解析处，或 aginx.net 的 DNS 控制台）——这一步需要你在 DNS 控制台操作
4. 回 GSC 点验证 → 通过后「Sitemap」→ 提交 `sitemap.xml`

**备选（免 DNS）——HTML 文件验证**：GSC 会给一个 `googleXXX.html`，把文件内容发给我，我部署到 web/ 即可（现在 nginx 自动 serve）。**或 meta 标签验证**：把 `<meta name="google-site-verification" ...>` 给我，我加进 web/index.html 的 head。

## 百度站长平台（中文市场，建议做）

1. 登录 [ziyuan.baidu.com](https://ziyuan.baidu.com)
2. 「用户中心」→「站点管理」→ 添加 `browser.aginx.net` → 选**文件验证**或 **meta 标签验证**
3. 验证文件/meta 内容发我部署
4. 验证后「普通收录」→「sitemap 提交」→ 填 `https://browser.aginx.net/sitemap.xml`（百度支持 XML sitemap，会提示推荐 txt/API，XML 也能用）

## Bing Webmaster Tools（最省事：从 Google 导入）

1. 登录 [bing.com/webmasters](https://www.bing.com/webmasters)
2. 如果有 GSC 验证 →「从 Google Search Console 导入」一键带入属性 + sitemap
3. 没有 GSC → 独立验证（DNS TXT / 文件 / meta，同上）

## 验证要点

- 域名级验证（DNS TXT）同时覆盖所有子路径，比单页验证稳
- GSC 收录后可用「网址检查」工具手动请求抓取 `/blog/` 页面加速索引
- 百度对 `/blog/` 中文内容收录通常较快（约 1-2 周）；Google 慢一些但双语 hreflang 已就绪

## 需要你操作 / 提供

| 步骤 | 谁做 |
|---|---|
| 创建 GSC/百度/Bing 资源 + 获取验证码 | 你（登录各控制台） |
| DNS TXT 添加（若选 DNS 验证） | 你（DNS 控制台） |
| 验证文件 / meta 标签部署 | 我（发我内容即部署） |
| 提交 sitemap + 请求索引 | 你（控制台点几下） |
