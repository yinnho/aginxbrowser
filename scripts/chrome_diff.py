#!/usr/bin/env python3
"""Compare the engine's layout against headless Chrome on a real URL.

Chrome is driven over the DevTools protocol (needs `pip install websockets`);
each engine is an already-running aginxbrowser server. For the same page it
extracts the boxes of paragraphs, headings, list items, tables, figures and
images, aligns them by tag + text prefix, and reports:

  one engine   : how many boxes match Chrome's left/width (2px tolerance)
  two engines  : A is the baseline, B the candidate - how many boxes B moved
                 closer to Chrome (improved) or away from it (REGRESSED)

  scripts/chrome_diff.py --engine off=18093 --engine on=18094 URL [URL...]
  scripts/chrome_diff.py --engine on=18094 --first-diff URL     # first big drift

Widths and lefts are compared, not tops or heights: those drift with font
metrics. Whole-page height is printed for orientation. Use --proxy when the
machine needs one to reach the sites (the engine takes its own via
AGINXBROWSER_PROXY).
"""
import argparse
import asyncio
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

try:
    import websockets
except ImportError:
    sys.exit("pip install websockets")

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
DEBUG_PORT = 9334
EXTRACT = r"""(()=>{const out=[];const norm=s=>(s||'').replace(/\s+/g,' ').trim().slice(0,24);
const seen={};
for(const e of document.querySelectorAll('p,h1,h2,h3,h4,li,dd,dt,blockquote,table,figure,img,pre')){
 const b=e.getBoundingClientRect(); if(b.width<=0||b.height<=0) continue;
 const k=e.tagName+'|'+norm(e.tagName==='IMG'?(e.getAttribute('alt')||e.getAttribute('src')):e.textContent);
 seen[k]=(seen[k]||0)+1; out.push([k+'#'+seen[k],Math.round(b.left),Math.round(b.top+scrollY),Math.round(b.width),Math.round(b.height)]);
 if(out.length>=500) break;}
return JSON.stringify({h:document.documentElement.scrollHeight,items:out});})()"""


async def chrome_eval(url, wait):
    target = json.load(urllib.request.urlopen(urllib.request.Request(
        f"http://127.0.0.1:{DEBUG_PORT}/json/new?about:blank", method="PUT")))
    async with websockets.connect(target["webSocketDebuggerUrl"], max_size=64 << 20) as ws:
        n = [0]

        async def call(method, params=None):
            n[0] += 1
            await ws.send(json.dumps({"id": n[0], "method": method, "params": params or {}}))
            while True:
                r = json.loads(await asyncio.wait_for(ws.recv(), 60))
                if r.get("id") == n[0]:
                    return r

        await call("Page.enable")
        await call("Emulation.setDeviceMetricsOverride",
                   {"width": 1280, "height": 720, "deviceScaleFactor": 1, "mobile": False})
        await call("Page.navigate", {"url": url})
        t0 = time.time()
        while time.time() - t0 < 25:
            try:
                if json.loads(await asyncio.wait_for(ws.recv(), 1)).get("method") == "Page.loadEventFired":
                    break
            except asyncio.TimeoutError:
                pass
        await asyncio.sleep(wait)
        r = await call("Runtime.evaluate", {"expression": EXTRACT, "returnByValue": True})
    urllib.request.urlopen(f"http://127.0.0.1:{DEBUG_PORT}/json/close/{target['id']}")
    return json.loads(r["result"]["result"]["value"])


def engine_eval(port, url):
    base = f"http://127.0.0.1:{port}"

    def rq(path, body):
        req = urllib.request.Request(base + path, json.dumps(body).encode(),
                                     {"Content-Type": "application/json"}, method="POST")
        return json.load(urllib.request.urlopen(req, timeout=180))

    sid = rq("/session/create", {"url": "about:blank", "ttl_secs": 600, "keepalive": True,
                                 "width": 1280, "height": 720})["session_id"]
    try:
        rq(f"/session/{sid}/navigate", {"url": url})
        return json.loads(rq(f"/session/{sid}/eval", {"script": EXTRACT, "timeout_ms": 30000})["result"])
    finally:
        try:
            rq(f"/session/{sid}/close", {})
        except Exception:
            pass


def boxes(r):
    return {k: (l, t, w, h) for k, l, t, w, h in r["items"]}


def ok(b, c):
    return abs(b[0] - c[0]) <= 2 and abs(b[2] - c[2]) <= 2


def report_one(name, chrome, eng):
    C, E = boxes(chrome), boxes(eng)
    both = [k for k in C if k in E]
    good = sum(ok(E[k], C[k]) for k in both)
    print(f"  {name}: matched={len(both)} box-ok={good} ({100 * good // max(len(both), 1)}%) height={eng['h']}")


def report_two(names, chrome, a, b):
    C, A, B = boxes(chrome), boxes(a), boxes(b)
    same = imp = reg = neither = 0
    regs, imps = [], []
    for k, c in C.items():
        if k not in A or k not in B:
            continue
        if A[k][0] == B[k][0] and A[k][2] == B[k][2]:
            same += 1
        elif ok(A[k], c) and ok(B[k], c):
            same += 1
        elif ok(B[k], c):
            imp += 1
            imps.append((k[:28], A[k][0], A[k][2], B[k][0], B[k][2], c[0], c[2]))
        elif ok(A[k], c):
            reg += 1
            regs.append((k[:28], A[k][0], A[k][2], B[k][0], B[k][2], c[0], c[2]))
        else:
            neither += 1
    print(f"  {names[0]}->{names[1]}: same={same} improved={imp} REGRESSED={reg} changed-but-neither={neither}"
          f" | height chrome={chrome['h']} {names[0]}={a['h']} {names[1]}={b['h']}")
    for x in regs[:5]:
        print("    REG (key, A l/w, B l/w, chrome l/w):", x)
    for x in imps[:3]:
        print("    imp:", x)


def first_diff(chrome, eng, limit=16):
    C = boxes(chrome)
    prev, shown = 0, 0
    print(f"  height chrome={chrome['h']} engine={eng['h']}")
    for k, l, t, w, h in eng["items"]:
        if k not in C:
            continue
        cl, ct, cw, ch = C[k]
        d = t - ct
        if abs(d - prev) > 80 or abs(h - ch) > 80 or abs(w - cw) > 80:
            print(f"  {k[:34]:34} engine(l={l},t={t},w={w},h={h}) chrome(l={cl},t={ct},w={cw},h={ch}) dTop={d}")
            shown += 1
        prev = d
        if shown >= limit:
            break


async def main(args):
    engines = [e.split("=", 1) for e in args.engine]
    profile = tempfile.mkdtemp(prefix="chrome_diff_")
    cmd = [args.chrome, "--headless=new", f"--remote-debugging-port={DEBUG_PORT}",
           "--window-size=1280,720", f"--user-data-dir={profile}", "--disable-gpu", "about:blank"]
    if args.proxy:
        cmd.insert(2, f"--proxy-server={args.proxy}")
    chrome = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    try:
        for url in args.urls:
            print(url)
            try:
                c = await chrome_eval(url, args.wait)
                results = [(n, engine_eval(int(p), url)) for n, p in engines]
            except Exception as ex:
                print("  ERR", str(ex)[:120])
                continue
            print(f"  chrome items={len(c['items'])}")
            if args.first_diff:
                first_diff(c, results[-1][1])
            elif len(results) == 1:
                report_one(results[0][0], c, results[0][1])
            else:
                report_two((results[0][0], results[1][0]), c, results[0][1], results[1][1])
    finally:
        chrome.send_signal(signal.SIGTERM)
        shutil.rmtree(profile, ignore_errors=True)


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("urls", nargs="+")
    ap.add_argument("--engine", action="append", required=True, metavar="NAME=PORT")
    ap.add_argument("--chrome", default=os.environ.get("CHROME", CHROME))
    ap.add_argument("--proxy", default=os.environ.get("CHROME_PROXY"))
    ap.add_argument("--wait", type=float, default=2.5, help="seconds to let page scripts settle in Chrome")
    ap.add_argument("--first-diff", action="store_true", help="list where the last engine drifts from Chrome")
    asyncio.run(main(ap.parse_args()))
