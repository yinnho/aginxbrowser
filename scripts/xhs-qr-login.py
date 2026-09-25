#!/usr/bin/env python3
"""Engine-born xhs QR login, one command (ledger chain of 2026-09-25).

  create (account-bound) session -> preload [mega signer, qr hook]
  -> navigate the login page -> capture the page's own qrcode/create
  data.url (fallback: one manual signed create) -> render the QR locally
  -> poll signed qrcode/status until code_status==2 -> the Set-Cookie
  (web_session family) lands in the session jar by itself, and the
  account layer writes it back under --account after every command.

The page's own status polling does not run in the engine (visibility
quirk), so status is polled by hand with _webmsxyw signing — that part
is load-bearing, do not "simplify" it away. Reuses the page's own QR
rather than minting a second one: repeated qrcode/create in one session
is the behavior fingerprint that got QR logins risk-checked on 09-22.

Usage:
  python3 scripts/xhs-qr-login.py --account xhs4
  python3 scripts/xhs-qr-login.py --account xhs4 --host 127.0.0.1:8198 --render-only
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

VENV = "/tmp/qrvenv"


def ensure_qrcode_lib():
    try:
        import qrcode  # noqa: F401
        return
    except ImportError:
        pass
    vpy = os.path.join(VENV, "bin", "python3")
    if sys.prefix == VENV:
        sys.exit("qrcode lib missing inside the venv too — pip install broken?")
    if not os.path.exists(vpy):
        print(f"[setup] creating {VENV} and installing qrcode ...", flush=True)
        subprocess.run([sys.executable, "-m", "venv", VENV], check=True)
        subprocess.run([os.path.join(VENV, "bin", "pip"), "install", "-q", "qrcode"], check=True)
    os.execv(vpy, [vpy] + sys.argv)


QR_HOOK = r"""
(() => {
  if (window.__qrhook) return 'already';
  window.__qrhook = true;
  const stash = (txt) => {
    try {
      const j = JSON.parse(txt);
      const u = j && j.data && j.data.url;
      if (!u) return;
      const p = new URL(u).searchParams;
      window.__xhs_qr = {
        url: u,
        qr_id: p.get('qrId') || p.get('qr_id') || '',
        code: p.get('xhs_code') || p.get('code') || ''
      };
    } catch (e) {}
  };
  const hit = (u) => /\/api\/sns\/web\/v1\/login\/qrcode\/create/.test(String(u));
  const O = XMLHttpRequest.prototype.open, S = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (m, u) {
    this.__qr_u = String(u);
    return O.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    if (hit(this.__qr_u)) this.addEventListener('loadend', () => stash(this.responseText));
    return S.apply(this, arguments);
  };
  const F = window.fetch;
  window.fetch = function (u) {
    const us = String(typeof u === 'object' && u ? u.url : u);
    const p = F.apply(this, arguments);
    if (hit(us)) return p.then((r) => { r.clone().text().then(stash).catch(() => {}); return r; });
    return p;
  };
  return 'hooked';
})()
"""

MANUAL_CREATE = r"""
(async () => {
  const r = await fetch('https://edith.xiaohongshu.com/api/sns/web/v1/login/qrcode/create', {
    method: 'POST', credentials: 'include',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ qr_type: 1 })
  });
  return await r.text();
})()
"""

STATUS_EVAL = r"""
(async () => {
  const p = '/api/sns/web/v1/login/qrcode/status?qr_id=' + encodeURIComponent(%s) +
            '&code=' + encodeURIComponent(%s);
  const sg = window._webmsxyw(p, undefined);
  const r = await fetch('https://edith.xiaohongshu.com' + p, {
    credentials: 'include',
    headers: { 'x-s': String(sg['X-s']), 'x-t': String(sg['X-t']) }
  });
  return await r.text();
})()
"""


def fail(msg):
    sys.exit(f"error: {msg}")


class Engine:
    def __init__(self, host):
        self.base = f"http://{host}"

    def call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            self.base + path, data=data, method=method,
            headers={"content-type": "application/json"} if data else {},
        )
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return json.loads(r.read().decode() or "{}")
        except urllib.error.HTTPError as e:
            fail(f"{method} {path} -> HTTP {e.code}: {e.read().decode()[:300]}")
        except urllib.error.URLError as e:
            fail(f"{method} {path} -> {e.reason} (engine up on {self.base}?)")

    def eval(self, sid, script):
        return self.call("POST", f"/session/{sid}/eval", {"script": script}).get("result")


def load_mega_patch():
    here = os.path.dirname(os.path.abspath(__file__))
    flow = os.path.join(here, "..", "workflow", "xhs-post", "flow.json")
    with open(flow) as f:
        steps = json.load(f)["steps"]
    for st in steps:
        s = st.get("args", {}).get("script", "")
        if "_webmsxyw" in s and "__xs_installed" in s:
            return s
    fail(f"mega signing patch not found in {flow}")


def parse_qr_payload(text):
    j = json.loads(text)
    url = (j.get("data") or {}).get("url")
    if not url:
        fail(f"qrcode/create response has no data.url: {text[:200]}")
    qs = dict(urllib.parse.parse_qsl(urllib.parse.urlparse(url).query))
    qr_id = qs.get("qrId") or qs.get("qr_id")
    code = qs.get("xhs_code") or qs.get("code")
    if not qr_id or not code:
        fail(f"cannot pull qrId/xhs_code out of {url[:120]}")
    return {"url": url, "qr_id": qr_id, "code": code}


def main():
    import qrcode  # ensured before main runs

    ap = argparse.ArgumentParser(description="engine-born xhs QR login")
    ap.add_argument("--account", help="named account jar to log into (omit = guest session, dies with it)")
    ap.add_argument("--host", default=os.environ.get("AGINXBROWSER_HOST", "127.0.0.1:8199"))
    ap.add_argument("--login-url", default="https://www.xiaohongshu.com/login")
    ap.add_argument("--timeout", type=float, default=600, help="seconds to wait for scan+confirm")
    ap.add_argument("--interval", type=float, default=2.5, help="status poll cadence")
    ap.add_argument("--qr-wait", type=float, default=20, help="seconds to wait for the page's own QR before manual create")
    ap.add_argument("--out", help="QR png path (default /tmp/xhs-qr-<account>.png)")
    ap.add_argument("--no-open", action="store_true", help="do not `open` the QR image")
    ap.add_argument("--keep", action="store_true", help="keep the session after login")
    ap.add_argument("--render-only", action="store_true", help="stop after rendering the QR (smoke test)")
    args = ap.parse_args()

    if not args.account:
        print("[warn] no --account: cookies die with this guest session", flush=True)
    out = args.out or f"/tmp/xhs-qr-{args.account or 'guest'}.png"
    eng = Engine(args.host)

    body = {"ttl_secs": 1800, "use_proxy": False}
    if args.account:
        body["account"] = args.account
    sid = eng.call("POST", "/session/create", body)["session_id"]
    print(f"[1/5] session {sid}" + (f" (account={args.account})" if args.account else ""), flush=True)

    try:
        eng.call("POST", f"/session/{sid}/preload", {"scripts": [load_mega_patch(), QR_HOOK]})
        eng.call("POST", f"/session/{sid}/navigate", {"url": args.login_url})
        print("[2/5] preload installed, navigated to login page", flush=True)

        qr = None
        deadline = time.time() + args.qr_wait
        while time.time() < deadline:
            raw = eng.eval(sid, "JSON.stringify(window.__xhs_qr || null)")
            if raw and raw != "null":
                qr = json.loads(raw)
                break
            time.sleep(1)
        if qr:
            print("[3/5] captured the page's own QR", flush=True)
        else:
            print("[3/5] page QR did not appear — one manual signed create", flush=True)
            qr = parse_qr_payload(eng.eval(sid, MANUAL_CREATE))

        qrcode.make(qr["url"]).save(out)
        print(f"[4/5] QR rendered -> {out}", flush=True)
        print(f"      payload: {qr['url'][:100]}", flush=True)
        if not args.no_open:
            subprocess.run(["open", out], check=False)
        if args.render_only:
            print("[render-only] stopping before status polling", flush=True)
            return
        print("      scan with the xhs app and confirm on the phone ...", flush=True)

        status_js = STATUS_EVAL % (json.dumps(qr["qr_id"]), json.dumps(qr["code"]))
        last = None
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            text = eng.eval(sid, status_js)
            try:
                st = json.loads(text).get("data", {}).get("code_status")
            except (json.JSONDecodeError, AttributeError):
                st = None
            if st != last:
                print(f"      code_status={st} ({text[:120] if st is None else {0: 'waiting', 1: 'scanned', 2: 'confirmed', 3: 'expired'}.get(st, st)})", flush=True)
                last = st
            if st == 2:
                break
            if st in (3, 471):
                fail("QR expired or scan rejected — rerun for a fresh one")
            time.sleep(args.interval)
        else:
            fail(f"no confirmation within {args.timeout}s")

        cookies = eng.call("GET", f"/session/{sid}/cookies")
        if "web_session=" not in json.dumps(cookies):
            fail("code_status=2 but no web_session in the jar — write-back broken?")
        print(f"[5/5] logged in — web_session landed"
              + (f" and is persisted under account '{args.account}'" if args.account else " (guest jar!)"),
              flush=True)
    finally:
        if not args.keep:
            eng.call("POST", f"/session/{sid}/close")
            print(f"      session {sid} closed", flush=True)


if __name__ == "__main__":
    ensure_qrcode_lib()
    main()
