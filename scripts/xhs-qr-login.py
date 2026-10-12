#!/usr/bin/env python3
"""Optional CLI around workflow/xhs-login — automation should call the flow.

  POST /session/create {"account": "..."}
  POST /flow/run {"name": "xhs-login", "session_id": "..."}
  # scan receipt saved.qr_data / saved.qr_shot / /live?session=

This script only: create session → flow/run → write qr_data to a PNG via
an SVG raster fallback (qlmanage on macOS) or print the data URL — so a
human can `open` a file. It does NOT own login logic anymore.
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request


def call(host, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"http://{host}{path}",
        data=data,
        method=method,
        headers={"content-type": "application/json"} if data else {},
    )
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.loads(r.read().decode() or "{}")


def main():
    ap = argparse.ArgumentParser(description="thin CLI for workflow/xhs-login")
    ap.add_argument("--account")
    ap.add_argument("--host", default=os.environ.get("AGINXBROWSER_HOST", "127.0.0.1:8199"))
    ap.add_argument("--timeout-ms", type=int, default=120000)
    ap.add_argument("--out", default="/tmp/xhs-qr.png")
    ap.add_argument("--no-open", action="store_true")
    ap.add_argument("--mode", choices=("qr", "cookies"), default="qr")
    args = ap.parse_args()

    body = {"width": 1440, "height": 900}
    if args.account:
        body["account"] = args.account
    sid = call(args.host, "POST", "/session/create", body)["session_id"]
    print(f"session {sid}", flush=True)

    receipt = call(
        args.host,
        "POST",
        "/flow/run",
        {
            "name": "xhs-login",
            "session_id": sid,
            "vars": {"mode": args.mode, "login_timeout_ms": args.timeout_ms},
        },
    )
    print("status", receipt.get("status"), flush=True)
    saved = receipt.get("saved") or {}
    qr_data = saved.get("qr_data")
    if isinstance(qr_data, str) and qr_data.startswith("data:image/svg+xml;base64,"):
        svg = base64.b64decode(qr_data.split(",", 1)[1])
        svg_path = args.out.rsplit(".", 1)[0] + ".svg"
        open(svg_path, "wb").write(svg)
        # macOS quicklook → png for `open`
        out_dir = tempfile.mkdtemp()
        subprocess.run(
            ["qlmanage", "-t", "-s", "500", "-o", out_dir, svg_path],
            check=False,
            capture_output=True,
        )
        thumb = os.path.join(out_dir, os.path.basename(svg_path) + ".png")
        if os.path.exists(thumb):
            os.replace(thumb, args.out)
            print(f"qr → {args.out}", flush=True)
            if not args.no_open:
                subprocess.run(["open", args.out], check=False)
        else:
            print(f"qr svg → {svg_path} (open this / use /live?session={sid})", flush=True)
            if not args.no_open:
                subprocess.run(["open", svg_path], check=False)
    else:
        print(
            f"no qr_data in receipt (mode={args.mode}?); use /live?session={sid}",
            flush=True,
        )
        print(json.dumps(receipt, ensure_ascii=False)[:500], flush=True)

    if receipt.get("status") != "ok":
        sys.exit(1)


if __name__ == "__main__":
    try:
        main()
    except urllib.error.URLError as e:
        sys.exit(f"engine unreachable: {e}")
    except Exception as e:
        sys.exit(f"error: {e}")
