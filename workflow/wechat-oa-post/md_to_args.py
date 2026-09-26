#!/usr/bin/env python3
"""md -> wechat-oa-post args_json 转换器(模板驱动)。

用法:
  python3 md_to_args.py article.md --title "标题" [--digest 摘要] \
      [--cover cover.png] [--template templates/default.html]

输出 args_json 到 stdout,直接喂 flow 的 vars.args_json。
模板规则见 templates/default.html 头注释:inline style 唯一可信,
外链不加 <a>(微信会剥),用 link 块上色成纯文本。

md 支持面(刻意小,文章不是网页):
  # 标题   → 跳过(标题走 args.title,公众号自己渲染)
  ## 小节  → h2 块
  段落     → p 块;段内 **粗体** → strong 块;裸 URL/域名 → link 块
"""
import argparse, base64, html, json, re, sys
from pathlib import Path

def load_templates(path):
    src = Path(path).read_text(encoding="utf-8")
    out = {}
    for m in re.finditer(
            r'<template id="([\w-]+)">(.*?)</template>', src, re.S):
        out[m.group(1)] = m.group(2).strip()
    missing = {"container", "h2", "p", "strong", "link"} - out.keys()
    if missing:
        sys.exit(f"template missing blocks: {sorted(missing)}")
    return out

def inline(text, tp):
    """转义后做行内变换:**粗体**、裸 URL/域名。"""
    t = html.escape(text)
    t = re.sub(r"\*\*(.+?)\*\*",
               lambda m: tp["strong"].replace("{{text}}", m.group(1)), t)
    t = re.sub(r"(?<![\w/])((?:https?://)?(?:[\w-]+\.)+(?:com|net|org|io|dev|cn)"
               r"(?:/[\w./-]*)?)",
               lambda m: tp["link"].replace("{{text}}", m.group(1)), t)
    return t

def convert(md, tp):
    blocks = []
    seen_p = False
    h_i = 0
    for raw in md.split("\n\n"):
        b = raw.strip()
        if not b or b.startswith("# "):
            continue
        if b.startswith("## "):
            h_i += 1
            block = tp["h2"].replace("{{text}}", inline(b[3:].strip(), tp))
            block = block.replace("{{n}}", f"{h_i:02d}")
            blocks.append(block)
        else:
            text = inline(b.replace("\n", ""), tp)
            kind = "lead" if (not seen_p and "lead" in tp) else "p"
            blocks.append(tp[kind].replace("{{text}}", text))
            seen_p = True
    return (tp["container"].replace("{{header}}", tp.get("header", ""))
            .replace("{{blocks}}", "\n".join(blocks)))

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("md")
    ap.add_argument("--title", required=True)
    ap.add_argument("--digest", default="")
    ap.add_argument("--cover")
    ap.add_argument("--template",
                    default=str(Path(__file__).parent / "templates/default.html"))
    a = ap.parse_args()
    tp = load_templates(a.template)
    out = {"title": a.title,
           "content_html": convert(Path(a.md).read_text(encoding="utf-8"), tp),
           "digest": a.digest,
           "cover_b64": base64.b64encode(Path(a.cover).read_bytes()).decode()
           if a.cover else ""}
    json.dump(out, sys.stdout, ensure_ascii=False)
    print()

if __name__ == "__main__":
    main()
