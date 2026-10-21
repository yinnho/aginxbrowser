# xhs-merchant-publish — 千帆/Ark publish_render 签名链探测

> **2026-10-21 重写。** 300015 通关后（原生 mnsv2 + Chrome 请求头顺序），页面自己的签名 HTTP 客户端就能签 `publish_render`，
> 不再需要 `xhs-sign` 配方（门闩/镜像/桥接那一整套已删，配方回到 C 端 flow 用的精简版）。本 flow 现在=
> 进创建页 → 按内容找页面 SDK 的 HTTP 客户端 → 发一发体为 `{}` 的 `publish_render`。**不提交商品。**
> 回「请求详情不能为空 / 未找到执行流程」这类业务错误=签名链+环境层都放行；`300015`=环境杀；jar 死会在探测前被拦。

## 判决表（flow 的四条出路）

| 出路 | 触发 | 含义 |
|---|---|---|
| `saved.result`（绿） | `pr_status:200` 且无 300015 | 签名链通且过闸；附 seller/info jar 复查 |
| throw 非预期回执 | 406/401/超时 | 缺签（406）或 jar 死（401/-13004） |
| throw 门闩合成拒 | `signer-not-ready` | 请求从未上线（settle 契约未满足），jar 无恙 |
| throw 环境杀 | `300015` | 端点风控闸拒（run4 实测=当前常态出路）——jar 200 时代不毒，可复跑 |

探测体是 `'{}'`：回执规则载荷（code 200）或业务参数错（code≠200 非 300015）
都证明签名层+环境层已过——签名链的证据不依赖业务参数正确性。

## 前置

- **绿 jar**：先跑 `workflow/xhs-qianfan-login`（B 端 CAS QR 登录，落
  `xhs-qf-stable` 账号）。jar 死会在 create 页被弹去 login——flow 的
  wait_signer predicate 放行弹跳，signer 观测步报精确 url。
- fleet 面 **Chrome/145**（2026-10-20 起与 TLS 预设 chrome145 一致；此前 154 声明配 145 握手）
  （146–153/155/156 会被路由进 SIT 墙；此前 145 的 300015 是在旧引擎上测的）。persona_ua 与 fleet 同钉 145。

## 已知坑

- 签名器只铸在 create 页（itemark/qiankun），且可能只在 `window.proxy._webmsxyw`
  不在 `window._webmsxyw`——recipe 会镜像，flow 两处都认。
- 未签 `publish_render` = HTTP 406 + 毒 jar（-13004 + sec_poison_id）。
  recipe 的门闩对未签 PR 合成拒（永不上线），这是双保险的另一层。
- **rerun 冷启动饿死**（2026-10-17 二踩）：每次 rerun 重导航 ark home =
  冷启动忙页，紧随的 eval 15/30s 双超时。flow 已加 `where` 探测+分支跳过
  nav——已在 ark（暖页）直接进预检。
- 引擎侧 `AGINXBROWSER_SIGNER_GATE`（8129 已挂）是第二道门：engine 层
  持有 publish_render 直到页内签名器谓词真。
- `xhs-login` 是 C 端，不要混用；登录一律走 `xhs-qianfan-login`。

## 用法

```bash
# flow 自建会话（account=xhs-qf-stable, persona_ua=Chrome/145）
curl -sS -X POST "$BASE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"xhs-merchant-publish"}'

# 或挂已有会话
curl -sS -X POST "$BASE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"xhs-merchant-publish","session_id":"s_N"}'
```

## 下一步（真写上架，待用户点头）

1. 表单驱动：类目/标题/图片走页内 UI（HAR=回执真源，对照 Chrome 抓包）。
2. `batch_create_item` 接线（body 真值来自 10-11 成功链 + 新 HAR）。
3. 店状态（deactivated）与风控分面处理。
