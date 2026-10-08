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
| brand_id / brand_text | "30025069481" / "无品牌/无注册商标" | 品牌 Select 值/文案。发射步按官方面优先级取真选项（表单默认对象/搜索解析/dataSource）；无品牌家族走表单自己的官方默认（如本类目 3246379），args 里的 id 不再是唯一真源——实际发射的对象记在回执 brandUsed |
| brand_prop_name | "p-20000" | 类目品牌属性名 |
| shipping_value | "320100" | 发货地，按 value 在 shippingArea 活 dataSource 里找节点 |
| template_id | "67791771540" | 运费模板（当前店铺的） |
| sku_prop_name / sku_param | "skuParam_p-660820343" / "564110794" | 包装清单属性名 + 选项值。sku_param 必须是活 dataSource 里的真选项 value（skuAsyncSelect），flow 绑成 `[{value,text}]` 对象而非裸字符串 |
| sku_stock | 999 | 每 SKU 库存 |
| main_images / portrait_images | [url×5] | 主图 1:1 + 3:4 竖图 |
| desc_images | [{url,width,height}×n] | 详情图，620 宽按纵横比算高 |
| skus | [{value,price,img}×18] | value=销售属性文案，img=SKU 图 |

## 结构（16 步，线性）

```
preload blockqn2（掐 qn 微前端注入 + everyhelp widget）
→ navigate publish.htm?catId=
→ wait 表单就绪（GlobalStore + placeholder>5；登录重定向在此超时）
→ eval+wait 定时器探针（宏任务饿死检测——楔死是按会话的，死了就重跑）
→ eval fiber 树 stash（window.__F.*，缺字段当场抛；品牌 Select 故意不 stash）
→ eval 绑值（全官方 onChange；skuParam 先尽力；mainImagesGroup 双形状）
→ wait sku 行数进 model
→ eval skuParam 二遍：现走 fiber 找列定义实例 → 活 dataSource → 整行重绑 [{value,text}]
→ eval 旺铺 templateContent（完整 group 形态，真样本克隆）
→ eval 挂 submit.htm 应答钩子（fetch+XHR 双挂）
→ eval 品牌发射：官方面优先（默认对象/搜索命中/无品牌别名），否则点开下拉打字物化后发射
→ wait 引擎 catProp 落库（90s——给异步物化留窗）
→ eval 提交：fv.catProp 直写 __brandUsed 官方对象 + preflight + emit click
→ wait 应答或跳 success.htm
→ eval 解析回执 {success, item_id, form_error[]}
```

## 不显眼但都交过学费的点

1. **品牌实例必须现走现用，引擎写是异步的**。两次失败叠出来的规
   则：(a) fiber 树里有**两个** p-20000 实例，树走抓到的那只
   onChange 会写 `{"undefined":null}` 进引擎——必须从可见 input
   （placeholder 请输入）**向上**走 ≤40 级找；(b) 哪怕是对的实例，
   绑值引发的 re-render 也会让它**过期**——过期闭包写进死引擎绑
   定，同样 `{"undefined":null}`。所以发射步在同一个 eval 里现走
   fiber 现调 onChange。而引擎侧的 catProp 提交是**异步**的（~1s
   落成 `{value:-1,text}` 形状，客户端校验认这个），wait 步等它落
   库后才进下一步。
2. **品牌官方面要靠「点开+打字」物化**（2026-10-08 干跑 s_82-s_87
   实测）。fresh 表单上品牌 Select 的 value 是 **undefined**——官方默
   认选项对象（本类目 `{value:3246379,text:"无品牌"}`）要点开下拉
   **并在搜索框打字**后才物化；50 项 dataSource 是热牌列表（永远不
   含无品牌项），Fusion Next 的异步搜索无法合成触发。发射步的优先
   级：(a) value 已是对象——id 精确命中 / dataSource 命中 / 无品牌
   家族别名（表单自己的默认就是官方面）；(b) 物化——点开+打
   brand_text（无品牌家族打「无品牌」），400ms 轮询等 value 解析成
   对象（id 命中/文本命中/家族别名三判）；(c) dataSource 异步到位；
   (d) 88s 后 legacy 直射 args（10-04/10-07 三个商品就这么建的，服
   务器会规范化——但那是假绿路径，别依赖）。**两个坑**：物化过程
   中 value 先变成待定合成对象 `{value:-1,text}`——-1 是自定义输入
   标记不是官方面，isOpt() 全程拒收，多等 ~1s 拿解析完成的真对象
   （s_85 tick 1 抓过 -1，engine 靠 text 救回来了，但 __brandUsed 记
   -1 就会把垃圾 id 绑进提交）；发射的对象记在 `window.__brandUsed`，
   提交步绑**它**，不从 args 重算。轮询 88s 上限卡在 wait 步 90s 内。
3. **skuParam 的选项面在表格列定义的 fiber 实例上，且表格渲染后才
   存在**（2026-10-08 干跑定位）。stash 步（绑值前）和绑值步当时都
   摸不到它——`eng.getComponent()` 在 registry 里是个空壳（props 为
   空），真身是 fiber 树上 name=sku_prop_name 的列定义实例，其
   `props.dataSource` 就是官方选项（本类目 4 项，含
   564110794|清洁工具包）。所以 flow 加了**二遍步**：sku 行落定后现
   走 fiber 找列定义 → 选项在 → 整行重绑 `[{value,text}]`。裸字符串
   的下场有据可查：我们这个类目服务器**静默丢弃**（商品 1091036652226
   编辑页回读 18 行 skuParam 全空），商家那个类目直接
   `CHK_SKU_PARAM_REQUIRED_ERROR` 24 连——对象形状是唯一能落库的。
   值语义照抄官方：dataSource 里 value 是字符串就绑字符串，回读规范
   化成数字是服务器的事，**别立法「必须是数字」**；只改适用行，18 行
   全表硬灌也行（本表就是），别的类目注意 24/49 的部分适用。
4. **catProp 两端都是原生对象**（10-05 推翻 10-04 判决）。10-04 真机
   用纯字符串建品成功，误判「服务器要字符串」；#203 商家回读已建
   商品，服务端把字符串规范化成 `{value:负数,text:"30025069481"}`
   ——id 数字进了显示文本槽，品牌中文名没了，假绿。商家原生链路
   实测形状两端一致：手选品牌后 formValues、组件 getProps、原生提
   交的 jsonBody.catProp 全是
   `{value:30025069481,text:"无品牌/无注册商标"}`（value 是**数字**），
   原生校验器 exec/isValid 也认。发射②直写这个对象；同样**不碰
   onChange**——任何 onChange 都会重新触发异步规范化，在 POST 序列
   化前把对象盖掉。数字/字符串在此处也是 normalize 不立法：纯数
   字串转 Number，非数字串原样透传。
5. **mainImagesGroup 双形状**。官方 onChange 收裸数组，序列化后服务器
   读成空（1:1主图为必填项）；要 `{images:[{url}]}` 对象——component
   `setProps` + `emit('change')` + fv 直写三件套。flow 两步都做。
6. **旺铺 desc 用完整 group 形态**。精简 picGroup 形态服务器报
   「保存详情数据失败」；descType=0 绕不过。模板是从商家真草稿
   （draft.htm?dbDraftId=…）里扒的：`type:"group"`、bizName 图文模
   块、单 pic 组件 background-image 直挂 URL。手拼格式过不了，别再
   逆向压缩码。
7. **XSRF 真源是 `window.csrfToken`**（`{tokenName,tokenValue}`），
   页面自己发的。排查 token 类拒绝别去翻 `.taobao.com` 同名宽域
   cookie——同名 cookie 存在且像真的，但不是提交用的那份。

## 回执怎么读

- `status:ok` + `saved.outcome`：服务器判决在里面。`outcome.success`
  true 时有 `item_id`（成功页 primaryId）；false 时 `form_error` 是
  `[字段:code:msg]` 数组——**这是服务器的明确拒绝，不是流失败**，按
  数组修 args 再跑（重跑=新商品，确认旧的那条真没落库）。见到
  SKU/CHK 类错误先看第 12 步 preflight 里的形状标记：`skuParam`
  （`object-from-dataSource` / `legacy-naked-string`）和 `brandVia`
  （`verified-default` / `verified-default-alias` / `verified-typed*` /
  `armed-fired` / `legacy-direct`）——落在 legacy 分支说明当次没拿到
  可验证的官方面，先查登录态/类目再谈 args。
- `status:failed`：传输/形状错误。看 `failed_step` + 回执截图；楔死
  （第 5 步超时）重跑即可，按会话的病。

## 真机 receipt

**对象形状真发**（2026-10-08，s_88 克隆 s_76，`/flow/run` 全 16 步含提交，
64s）：itemId **1090268221238**，success.htm primaryId 直落。编辑页回读
（服务器持久态）：**skuParam 18/18 行对象**、0 裸串 0 空，
row0=`[{value:564110794,text:"清洁工具包"}]`（服务器把 value 规范化成了
**数字**——dataSource 绑的是字符串，回读变数字，印证「别立法 number/
string」）；catProp=`{p-20000:{value:30025069481,text:"无品牌/无注册商标"}}`
——value=id 数字、text=中文名，**与商家手选的原生形状完全一致**，不是
旧假绿的 `{value:负数,text:"<id数字>"}`。#203 诉求①（品牌+skuParam 对象
形状）服务端实证闭环。小瑕疵：preflight 快照里 brandVia 停在
`armed-waiting-async`——发射轮询是异步的，快照时刻对象还没解析完（或
引擎侧 catProp 已带干净对象，无需再发射）；标记是时点值，**回执+回读
才是终态真相**，见到这个标记别急着判 legacy。

**对象形状干跑**（2026-10-08，会话 s_87（克隆 s_76 tb-pub 登录态），
16 步跑到提交前一步停）：`skuParam=object-from-dataSource`，
row0=`[{value:"564110794",text:"清洁工具包"}]`，**18/18 行对象形
状**；`brandVia=verified-default-alias`，`brandUsed={value:3246379,
text:"无品牌"}`（本类目官方默认，物化后 tick 3 发射，-1 待定对象被
isOpt 拒收一次）；catProp 落 `{p-20000:3246379}`；主图 5/竖图 5/详情
模板 1 全在。同场证据：商品 1091036652226（旧 naked 绑法建的）编辑页
回读 18 行 skuParam 全空=服务器静默丢弃裸串的实锤。

**flow 本体**（2026-10-04，会话 s_9，15 步全绿）：
`status:ok`，submit.htm 200 返回 `globalMessage.type=success` +
`successUrl=…primaryId=1090234620927`。编辑页回读：title/price
16.60/quantity 17982/18 SKU（S06=91.83 原样）/主图 5/详情
templateContent 5446 字节/品牌 30025069481（服务器回读形状
`{value:-31224754800,text:"30025069481"}`，负 value 是服务器分配
的，正常——当时是 legacy 直射路径）。

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
