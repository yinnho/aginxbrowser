#!/usr/bin/env bash
# 抓取知乎专栏文章 (zhuanlan.zhihu.com/p/xxx)。
#
# 用法:
#   ./zhihu.sh <文章URL> <__zse_ck值>
#
# 示例:
#   ./zhihu.sh "https://zhuanlan.zhihu.com/p/688930250" \
#       "005_qvhGt2mNygJDmKTPXnMBrAQx3VL2ayASgSNxYtRI=..."
#
# 知乎风控要点 (实测结论):
#   - 专栏文章是公开内容, 无需登录; 但知乎要求带 __zse_ck cookie 才放行 (否则 403)
#   - __zse_ck 一个 cookie 就够, 不需要 d_c0 / 登录态 / 住宅代理
#   - 同一个 __zse_ck 对所有专栏文章通用, 有效期约一年
#   - 正文在初始 HTML 的 <script id="js-initialData"> JSON 里 (SSR),
#     直接解析, 不依赖 JS 渲染, 绕过 aginxbrowser DOM 不完整的问题
#
# __zse_ck 获取 (约一年一次):
#   浏览器打开任意知乎专栏 -> F12 -> Application -> Cookies -> .zhihu.com
#   -> 复制 __zse_ck 的 Value
#   (首次访问知乎会弹易盾验证码, 人工过一次后 __zse_ck 长期有效;
#    aginxbrowser 自动过验证码需 OCR, 见 docs/ANTI_BOT.md 的 P3)
#
# 服务地址可通过 AGINXBROWSER_ADDR 环境变量覆盖 (默认本机 8089)。

set -euo pipefail

URL="${1:?用法: $0 <文章URL> <__zse_ck>}"
ZSECK="${2:?缺少 __zse_ck (浏览器 F12 -> Cookies -> .zhihu.com -> __zse_ck)}"
ADDR="${AGINXBROWSER_ADDR:-127.0.0.1:8089}"

# 从 js-initialData 同步提取正文 (不走 async/JS 渲染, 稳定可靠)
SCRIPT='(()=>{
  const s=document.querySelector("script#js-initialData");
  if(!s) return {err:"no initialData (cookie 失效或被风控?)"};
  const data=JSON.parse(s.textContent);
  const arts=data.initialState?.entities?.articles||{};
  const article=Object.values(arts)[0];
  if(!article) return {err:"文章未找到", title:document.title};
  const strip=html=>(html||"").replace(/<[^>]+>/g,"").replace(/&nbsp;/g," ").replace(/&amp;/g,"&").replace(/&lt;/g,"<").replace(/&gt;/g,">").trim();
  return {
    title: article.title,
    author: article.author?.name,
    voteup: article.voteupCount,
    comment: article.commentCount,
    created: article.created,
    bodyLen: strip(article.content).length,
    body: strip(article.content)
  };
})()'

python3 -c "
import json,sys
req={'url':sys.argv[1],'script':sys.argv[2],'cookies':['__zse_ck='+sys.argv[3]]}
sys.stdout.write(json.dumps(req,ensure_ascii=False))
" "$URL" "$SCRIPT" "$ZSECK" > /tmp/zhihu_req.json

curl -s -X POST "http://$ADDR/eval" -H "Content-Type: application/json" -d @/tmp/zhihu_req.json --max-time 60 \
  | python3 -c "
import json,sys
d=json.load(sys.stdin)
r=d.get('result')
if not isinstance(r,dict) or r.get('err'):
    print('抓取失败:', json.dumps(d,ensure_ascii=False)[:300]); sys.exit(1)
import datetime
ts=datetime.datetime.fromtimestamp(r.get('created',0)).strftime('%Y-%m-%d') if r.get('created') else '?'
print('='*60)
print('标题:', r.get('title'))
print('作者:', r.get('author'))
print('发布:', ts, ' | 赞:', r.get('voteup'), ' | 评论:', r.get('comment'))
print('正文:', r.get('bodyLen'), '字符')
print('='*60)
print(r.get('body',''))
"
