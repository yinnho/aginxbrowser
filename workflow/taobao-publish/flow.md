# taobao-publish — 淘宝商家发布流（v2 publish.htm 全链）

从已登录的 cookie 状态出发，一条 flow 走完：进发布表单 → 官方路径
绑全部字段 → 旺铺详情 → 单次提交 → 解析回执。2026-10-04 真机闭环过
一遍（itemId 1087198507341，18 SKU 全落库），flow 把当天的临时脚本
固化：`/tmp/stash_all.py`、`bind_official.py`、`desc_set2.py`、
`go_submit.py` 全部收编。

## 前置

- 登录态：先跑 `taobao-login`。cookie 落 `.taobao.com`，本引擎会话共
  享，flow 自己 create 的新会话直接可用。登录过期的话第 3 步
  （form_ready）超时，回执截图就是登录页——不是 bug。
- 图片：输入是**已经传好的 alicdn URL**（sucai 素材上传是另一条链，
  见下方「未收编」）。
- 类目：按贴膜类（201305805）的表单形状写死了一批字段名——品牌
  Select `p-20000`、包装清单 `skuParam_p-660820343`、按钮
  `button-submit`。换类目要改 `brand_prop_name`/`sku_prop_name` 两个
  args，并接受 stash 步的缺失字段会 fail-loudly。

## args（16 个，缺一个整条流拒跑）

| key | 例值 | 说明 |
|---|---|---|
| cat_id | "201305805" | 发布表单类目 |
| title | "适用25款昊铂HL…" | 30 汉字内 |
| price / quantity | "16.60" / "17982" | 一口价 + 总库存（字符串） |
| brand_id / brand_text | "30025069481" / "无品牌/无注册商标" | 品牌 Select 值/文案 |
| brand_prop_name | "p-20000" | 类目品牌属性名 |
| shipping_value | "320100" | 发货地，按 value 在 shippingArea 活 dataSource 里找节点 |
| template_id | "67791771540" | 运费模板（当前店铺的） |
| sku_prop_name / sku_param | "skuParam_p-660820343" / "564110794" | 包装清单属性名 + 选项值（清洁工具包） |
| sku_stock | 999 | 每 SKU 库存 |
| main_images / portrait_images | [url×5] | 主图 1:1 + 3:4 竖图 |
| desc_images | [{url,width,height}×n] | 详情图，620 宽按纵横比算高 |
| skus | [{value,price,img}×18] | value=销售属性文案，img=SKU 图 |

## 结构（15 步，线性）

```
preload blockqn2（掐 qn 微前端注入）
→ navigate publish.htm?catId=
→ wait 表单就绪（GlobalStore + placeholder>5；登录重定向在此超时）
→ eval+wait 定时器探针（宏任务饿死检测——楔死是按会话的，死了就重跑）
→ eval fiber 树 stash（window.__F.*，缺字段当场抛；品牌 Select 故意不 stash）
→ eval 绑值（全官方 onChange + mainImagesGroup 双形状）
→ wait sku 行数进 model
→ eval 旺铺 templateContent（完整 group 形态，真样本克隆）
→ eval 挂 submit.htm 应答钩子（fetch+XHR 双挂）
→ eval 品牌发射①：从可见 input 现走 fiber 找实例 + onChange
→ wait 引擎 catProp 异步落库（{p-20000:{value:-1,text}} 形状）
→ eval 品牌发射②：fv.catProp 直写原生对象 + preflight + emit click
→ wait 应答或跳 success.htm
→ eval 解析回执 {success, item_id, form_error[]}
```

## 三个不显眼但都交过学费的点

1. **品牌实例必须现走现用，引擎写是异步的**。两次失败叠出来的规
   则：(a) fiber 树里有**两个** p-20000 实例，树走抓到的那只
   onChange 会写 `{"undefined":null}` 进引擎——必须从可见 input
   （placeholder 请输入）**向上**走 ≤40 级找；(b) 哪怕是对的实例，
   绑值引发的 re-render 也会让它**过期**——过期闭包写进死引擎绑
   定，同样 `{"undefined":null}`。所以发射步在同一个 eval 里现走
   fiber 现调 onChange。而引擎侧的 catProp 提交是**异步**的（~1s
   落成 `{value:-1,text}` 形状，客户端校验认这个），wait 步等它落
   库后才进下一步。
2. **catProp 两端都是原生对象**（10-05 推翻 10-04 判决）。10-04 真机
   用纯字符串建品成功，误判「服务器要字符串」；#203 商家回读已建
   商品，服务端把字符串规范化成 `{value:负数,text:"30025069481"}`
   ——id 数字进了显示文本槽，品牌中文名没了，假绿。商家原生链路
   实测形状两端一致：手选品牌后 formValues、组件 getProps、原生提
   交的 jsonBody.catProp 全是
   `{value:30025069481,text:"无品牌/无注册商标"}`（value 是**数字**），
   原生校验器 exec/isValid 也认。发射②直写这个对象；同样**不碰
   onChange**——任何 onChange 都会重新触发异步规范化，在 POST 序列
   化前把对象盖掉。
3. **mainImagesGroup 双形状**。官方 onChange 收裸数组，序列化后服务器
   读成空（1:1主图为必填项）；要 `{images:[{url}]}` 对象——component
   `setProps` + `emit('change')` + fv 直写三件套。flow 两步都做。
4. **旺铺 desc 用完整 group 形态**。精简 picGroup 形态服务器报
   「保存详情数据失败」；descType=0 绕不过。模板是从商家真草稿
   （draft.htm?dbDraftId=…）里扒的：`type:"group"`、bizName 图文模
   块、单 pic 组件 background-image 直挂 URL。手拼格式过不了，别再
   逆向压缩码。

## 回执怎么读

- `status:ok` + `saved.outcome`：服务器判决在里面。`outcome.success`
  true 时有 `item_id`（成功页 primaryId）；false 时 `form_error` 是
  `[字段:code:msg]` 数组——**这是服务器的明确拒绝，不是流失败**，按
  数组修 args 再跑（重跑=新商品，确认旧的那条真没落库）。
- `status:failed`：传输/形状错误。看 `failed_step` + 回执截图；楔死
  （第 5 步超时）重跑即可，按会话的病。

## 真机 receipt

**flow 本体**（2026-10-04，会话 s_9，15 步全绿）：
`status:ok`，submit.htm 200 返回 `globalMessage.type=success` +
`successUrl=…primaryId=1090234620927`。编辑页回读：title/price
16.60/quantity 17982/18 SKU（S06=91.83 原样）/主图 5/详情
templateContent 5446 字节/品牌 30025069481（服务器回读形状
`{value:-31224754800,text:"30025069481"}`，负 value 是服务器分配
的，正常）。

**固化前的等价脚本链**（2026-10-04，会话 s_19，itemId
1087198507341）：第 7 次提交成功（前 6 次被服务器逐层拒绝后修
正），编辑页回读 18 SKU（16.60–98.80）全在。回读一律走
`publish.htm?catId=…&itemId=…`——千牛出售中列表的 mtop async 轮询
和商品公开页都会撞 x5sec 验证码，编辑页是干净通道。

## 未收编

- **图片上传**（36 张本地文件 → sucai PicUpload → stream-upload →
  URL 回执）还是 /tmp 脚本，待固化成 `taobao-media-upload` flow 后与
  本流串成「本地文件 → 在售」全链。
- 抖店侧发布链未开工（#203 的另一半）。
