# tmall-publish-custom — 天猫旗舰店多规格发布（颜色分类轴自定义值）

淘宝 `taobao-publish-custom` 的天猫版，**真提交**。先 `taobao-upload` 传图（图片空间按卖家号算，天猫店号自己的）。

## 天猫和淘宝表单的差别（2026-10-21 在婧途车品旗舰店摸出来的）

| 项 | 淘宝 | 天猫 |
|---|---|---|
| 发布页 | `item.upload.taobao.com/sell/v2/publish.htm` | **`sell.publish.tmall.com/tmall/publish.htm`**（淘宝域名对天猫卖家报"请到天猫卖家中心操作"；`item.upload.tmall.com` 直接 404） |
| 标题值 | 字符串 | **`{title:[文字], prefix:''}`** |
| 品牌 | `catProp['p-20000']={value,text}`，要无品牌/搜索物化 | 字段在 `itemProp`，下拉只有**店铺自己的品牌**（婧途 `-37965418903`），按活实例（从输入框往上走 fiber）调 onChange，再补写 `fv.itemProp` 对象形状 |
| 必填多一项 | — | **透明素材图**（`guideImageGroup`，`{whiteBgImage:[{url}]}`，PNG 800×800 透明底，≤3MB） |
| 发货地/模板/详情 | `shippingArea` / `template` / `desc` | `location` / `tmExtractWay` / `descRepublicOfSell`（页面默认值即可，stash 里不再要求前三者） |
| 提交 | replica XHR | 页面自己的 `pageState.submit`，第一下点击就直接提交（也可能先过 `previewDraftSubmit` 关卡，flow 两种都等） |

规格、规格图、商家编码、颜色分类自定义值的做法同 `taobao-publish-custom`。

## 跑法

```
POST /flow/run {"name":"tmall-publish-custom","session_id":"<天猫店登录会话>",
  "vars":{"args":{cat_id,title,brand_id,brand_text,brand_prop_name,white_bg_image,
                  main_images[],portrait_images[],desc_images[{url,width,height}],
                  skus[{text,price,stock,img,code,pkg_value,pkg_text}]}}}
```

登录：`taobao-login` 流程用天猫店铺账号扫码（会话挂 `account` + `keepalive`）。进"商品 → 发布商品"的真入口是
`upload.taobao.com/auction/sell.jhtml`（重定向到 `sell.publish.tmall.com/tmall/ai/category.htm`）。

## 验证记录

2026-10-21 真发：昊铂 HL 18 规格，`itemId=1092750818974`（`success.htm?...auctionStatus=-5`）。重开编辑页回读：
标题、18 行规格名与价格（天猫参考价）、规格图 18/18、商家编码、主图 5、品牌婧途、透明素材图都在。

**透明素材图是我用 SKU 图里的屏幕区域裁出来的圆角透明底 PNG（近似），不是美工抠图**；要换成正式素材，传好图后改 `white_bg_image` 重提（`item_param` 走编辑模式）。
